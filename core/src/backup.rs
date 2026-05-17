//! Backup manager for undo/rollback functionality.
//!
//! Creates timestamped backups of files and directories before mutation,
//! enabling restoration to the pre-mutation state. Backups are organized
//! under a root directory with subdirectories per job ID.
//!
//! # Security
//!
//! ## Backup Directory Symlink Check (FINDING #11)
//! The backup directory is verified to be a real directory (not a symlink)
//! during construction. This prevents an attacker from redirecting backups
//! to an arbitrary location via symlink substitution.
//!
//! ## Symlink Rejection in Copy Operations
//! The `copy_recursive` function explicitly rejects symlinks to prevent
//! symlink attack vectors. If a symlink is encountered during traversal,
//! the copy fails with an error.
//!
//! # Features
//!
//! - Supports both files and directories
//! - Preserves file permissions (including executable bit) on Unix systems
//! - Symlink rejection for security
//! - Automatic backup numbering to preserve multiple versions
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

    /// Recursively copies a file or directory, rejecting symlinks for security.
    ///
    /// # Security
    ///
    /// This function explicitly rejects symlinks to prevent symlink attack vectors.
    /// If a symlink is encountered during traversal, the copy fails with an error.
    ///
    /// # Metadata
    ///
    /// Preserves file permissions (including executable bit) on Unix systems.
    /// Directory permissions are set to platform defaults.
    #[cfg(unix)]
    fn copy_permissions(src: &Path, dst: &Path) -> std::io::Result<()> {
        let src_meta = std::fs::metadata(src)?;
        std::fs::set_permissions(dst, src_meta.permissions())?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn copy_permissions(_src: &Path, _dst: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
        // Check if src is a symlink using symlink_metadata (doesn't follow symlinks)
        let metadata = src.symlink_metadata()?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symlink detected: {} (symlinks not allowed for security)", src.display()),
            ));
        }

        if src.is_dir() {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let src_path = entry.path();
                let dst_path = dst.join(entry.file_name());
                Self::copy_recursive(&src_path, &dst_path)?;
            }
            // Preserve directory permissions on Unix
            Self::copy_permissions(src, dst)?;
            Ok(())
        } else {
            std::fs::copy(src, dst)?;
            // File permissions already preserved by std::fs::copy on Unix
            Ok(())
        }
    }

    /// Creates a backup of a file or directory before mutation.
    ///
    /// Copies `file_path` to `{backup_dir}/{job_id}/{filename}`.
    /// If a backup already exists, appends a numeric suffix (`.1`, `.2`, etc.)
    /// to preserve all pre-mutation states. The first backup (no suffix) always
    /// contains the original file state before any writes in this job.
    ///
    /// # Arguments
    ///
    /// * `file_path` — Path to the file or directory to back up
    /// * `job_id` — Job ID used as the backup subdirectory name
    ///
    /// # Returns
    ///
    /// The path to the backup file or directory.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the source
    /// does not exist or the copy fails.
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

        Self::copy_recursive(file_path, &backup_path)
            .map_err(|e| crate::Error::BackupError(e.to_string()))?;

        Ok(backup_path)
    }

    /// Restores a file or directory from a backup.
    ///
    /// Copies `backup_path` to `target_path`, overwriting the target.
    /// Handles both files and directories recursively.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// does not exist or the copy fails.
    pub fn restore(&self, backup_path: &Path, target_path: &Path) -> Result<()> {
        if !backup_path.exists() {
            return Err(crate::Error::BackupError(
                "Backup does not exist".to_string(),
            ));
        }

        // Use recursive copy to handle both files and directories
        BackupManager::copy_recursive(backup_path, target_path)
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

    #[test]
    fn test_backup_directory() {
        use crate::backup::BackupManager;

        let backup_dir = std::env::temp_dir().join("runtimo_backup_dir_test");
        let source_dir = std::env::temp_dir().join("runtimo_source_dir_test");
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&source_dir);

        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("file1.txt"), "content1").unwrap();
        std::fs::write(source_dir.join("file2.txt"), "content2").unwrap();

        let mgr = BackupManager::new(backup_dir.clone()).unwrap();
        let result = mgr.create_backup(&source_dir, "job123");

        assert!(result.is_ok(), "should backup directory: {:?}", result);
        let backup_path = result.unwrap();
        assert!(backup_path.exists());
        assert!(backup_path.join("file1.txt").exists());
        assert!(backup_path.join("file2.txt").exists());

        let content1 = std::fs::read_to_string(backup_path.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        std::fs::remove_dir_all(&backup_dir).ok();
        std::fs::remove_dir_all(&source_dir).ok();
    }

    #[test]
    fn test_backup_rejects_symlinks() {
        use crate::backup::BackupManager;

        let backup_dir = std::env::temp_dir().join("runtimo_backup_symlink_test");
        let source_dir = std::env::temp_dir().join("runtimo_source_symlink_test");
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&source_dir);

        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("file.txt"), "content").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let symlink_path = source_dir.join("evil_symlink");
            if symlink("/etc/passwd", &symlink_path).is_ok() {
                let mgr = BackupManager::new(backup_dir.clone()).unwrap();
                let result = mgr.create_backup(&source_dir, "job123");
                assert!(
                    result.is_err(),
                    "should reject directory containing symlinks"
                );
                let err = result.err().unwrap().to_string();
                assert!(err.contains("symlink"), "error should mention symlink: {}", err);
            }
        }

        std::fs::remove_dir_all(&backup_dir).ok();
        std::fs::remove_dir_all(&source_dir).ok();
    }

    #[test]
    fn test_restore_directory() {
        use crate::backup::BackupManager;

        let backup_dir = std::env::temp_dir().join("runtimo_restore_backup_test");
        let source_dir = std::env::temp_dir().join("runtimo_restore_source_test");
        let restore_dir = std::env::temp_dir().join("runtimo_restore_target_test");
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&restore_dir);

        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("file1.txt"), "content1").unwrap();
        std::fs::write(source_dir.join("file2.txt"), "content2").unwrap();

        let mgr = BackupManager::new(backup_dir.clone()).unwrap();
        let backup_result = mgr.create_backup(&source_dir, "job123");
        assert!(backup_result.is_ok());
        let backup_path = backup_result.unwrap();

        let restore_result = mgr.restore(&backup_path, &restore_dir);
        assert!(restore_result.is_ok(), "should restore directory: {:?}", restore_result);
        assert!(restore_dir.join("file1.txt").exists());
        assert!(restore_dir.join("file2.txt").exists());

        let content1 = std::fs::read_to_string(restore_dir.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        std::fs::remove_dir_all(&backup_dir).ok();
        std::fs::remove_dir_all(&source_dir).ok();
        std::fs::remove_dir_all(&restore_dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn test_backup_preserves_executable_bit() {
        use crate::backup::BackupManager;
        use std::os::unix::fs::PermissionsExt;

        let backup_dir = std::env::temp_dir().join("runtimo_backup_exec_test");
        let source_dir = std::env::temp_dir().join("runtimo_source_exec_test");
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&source_dir);

        std::fs::create_dir_all(&source_dir).unwrap();
        let script_path = source_dir.join("script.sh");
        std::fs::write(&script_path, "#!/bin/bash\necho hello").unwrap();
        // Set executable bit
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let mgr = BackupManager::new(backup_dir.clone()).unwrap();
        let result = mgr.create_backup(&source_dir, "job123");
        assert!(result.is_ok());

        let backup_path = result.unwrap();
        let backup_script = backup_path.join("script.sh");
        let backup_perms = std::fs::metadata(backup_script).unwrap().permissions();
        assert!(backup_perms.mode() & 0o111 == 0o111, "executable bit should be preserved");

        std::fs::remove_dir_all(&backup_dir).ok();
        std::fs::remove_dir_all(&source_dir).ok();
    }
}
