//! FileWrite capability — writes files with backup-before-mutate for undo support.
//!
//! Before overwriting an existing file, creates a backup via [`BackupManager`]
//! so the operation can be rolled back. Supports both overwrite and append modes.
//! Respects `dry_run` in the context to skip actual writes.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::FileWrite;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//! use std::path::PathBuf;
//!
//! let cap = FileWrite::new(PathBuf::from("/tmp/backups"));
//! let result = cap.execute(
//!     &json!({"path": "/tmp/output.txt", "content": "hello"}),
//!     &Context { dry_run: false, job_id: "job1".into() },
//! ).unwrap();
//!
//! assert!(result.success);
//! assert_eq!(std::fs::read_to_string("/tmp/output.txt").unwrap(), "hello");
//! ```

use crate::backup::BackupManager;
use crate::capability::{Capability, Context, Output};
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Maximum content size allowed for writing (100 MB).
/// Prevents disk exhaustion when agents attempt multi-gigabyte writes.
const MAX_WRITE_SIZE: usize = 100 * 1024 * 1024;

/// Arguments for the [`FileWrite`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWriteArgs {
    /// Path to the file to write.
    pub path: String,
    /// Content to write to the file.
    pub content: String,
    /// If true, append to the file instead of overwriting.
    #[serde(default)]
    pub append: bool,
}

/// Capability that writes file contents with backup-before-mutate.
///
/// If the target file exists, a backup is created before writing. If the file
/// does not exist, parent directories are created automatically. Supports
/// append mode and dry-run mode.
///
/// # Example
///
/// ```rust,ignore
/// use runtimo_core::capabilities::FileWrite;
/// use runtimo_core::capability::{Capability, Context};
/// use serde_json::json;
/// use std::path::PathBuf;
///
/// let cap = FileWrite::new(PathBuf::from("/tmp/backups"));
///
/// // Dry run — no file is written
/// let result = cap.execute(
///     &json!({"path": "/tmp/test.txt", "content": "hello"}),
///     &Context { dry_run: true, job_id: "job1".into() },
/// ).unwrap();
/// assert!(result.data["dry_run"].as_bool().unwrap());
///
/// // Append mode
/// let result = cap.execute(
///     &json!({"path": "/tmp/test.txt", "content": " world", "append": true}),
///     &Context { dry_run: false, job_id: "job2".into() },
/// ).unwrap();
/// ```
pub struct FileWrite {
    backup_mgr: BackupManager,
}

impl FileWrite {
    /// Creates a new FileWrite capability with the given backup directory.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// directory cannot be created.
    pub fn new(backup_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            backup_mgr: BackupManager::new(backup_dir)?,
        })
    }
}

impl Capability for FileWrite {
    fn name(&self) -> &'static str {
        "FileWrite"
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Creates automatic backups of existing files for undo support. Supports append mode."
    }

    /// Returns the JSON Schema for FileWrite arguments.
    ///
    /// Schema requires `"path"` and `"content"` strings; `"append"` is an
    /// optional boolean (defaults to false).
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "append": { "type": "boolean" }
            },
            "required": ["path", "content"]
        })
    }

    /// Validates the path argument using unified validation module.
    ///
    /// Write operations use relaxed path validation: the file need not exist
    /// (we create new files), but path traversal and prefix restrictions are
    /// enforced to prevent writes to sensitive directories.
    fn validate(&self, args: &Value) -> Result<()> {
        let args: FileWriteArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };

        validate_path(&args.path, &ctx).map_err(Error::SchemaValidationFailed)?;

        Ok(())
    }

    /// Writes content to the file, with backup if the file exists.
    ///
    /// In dry-run mode, returns success without writing. In append mode,
    /// content is appended to the existing file. Otherwise, the file is
    /// overwritten (after backup).
    ///
    /// # Errors
    ///
    /// Returns [`Error::ExecutionFailed`] if
    /// backup creation, directory creation, or file writing fails.
    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
        let args: FileWriteArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        if args.content.len() > MAX_WRITE_SIZE {
            return Err(Error::ExecutionFailed(format!(
                "Content too large: {} bytes (limit: {} bytes)",
                args.content.len(),
                MAX_WRITE_SIZE
            )));
        }

        let write_ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };

        let path = validate_path(&args.path, &write_ctx)
            .map_err(|e| Error::ExecutionFailed(format!("path validation: {}", e)))?;

        // Backup existing file before overwriting — enables undo via WAL
        let backup_path = if path.exists() {
            match self.backup_mgr.create_backup(&path, &ctx.job_id) {
                Ok(bp) => Some(bp),
                Err(e) => return Err(Error::ExecutionFailed(format!("backup: {}", e))),
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::ExecutionFailed(format!("mkdir {}: {}", parent.display(), e))
                })?;
            }
            None
        };

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "path": path.display().to_string(),
                    "content_length": args.content.len(),
                    "dry_run": true,
                    "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
                }),
                message: Some(format!(
                    "DRY RUN: would write {} bytes to {}",
                    args.content.len(),
                    path.display()
                )),
            });
        }

        if args.append {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| Error::ExecutionFailed(format!("open {}: {}", path.display(), e)))?;
            file.write_all(args.content.as_bytes())
                .map_err(|e| Error::ExecutionFailed(format!("write {}: {}", path.display(), e)))?;
        } else {
            std::fs::write(&path, &args.content)
                .map_err(|e| Error::ExecutionFailed(format!("write {}: {}", path.display(), e)))?;
        }

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "path": path.display().to_string(),
                "bytes_written": args.content.len(),
                "append": args.append,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
            }),
            message: Some(format!(
                "Wrote {} bytes to {}",
                args.content.len(),
                path.display()
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backup_dir() -> PathBuf {
        std::env::temp_dir().join("runtimo_fw_test")
    }

    #[test]
    fn writes_new_file() {
        let target = std::env::temp_dir().join("runtimo_fw_new.txt");
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");

        let result = cap
            .execute(
                &serde_json::json!({
                    "path": target.to_str().unwrap(),
                    "content": "hello from runtimo"
                }),
                &Context {
                    dry_run: false,
                    job_id: "t1".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hello from runtimo"
        );

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn dry_run_does_not_write() {
        let target = std::env::temp_dir().join("runtimo_fw_dry.txt");
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");

        cap.execute(
            &serde_json::json!({
                "path": target.to_str().unwrap(),
                "content": "should not exist"
            }),
            &Context {
                dry_run: true,
                job_id: "t2".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .expect("Execution failed");

        assert!(!target.exists());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");
        let err = cap
            .validate(&serde_json::json!({
                "path": "../../../etc/passwd",
                "content": "malicious"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("traversal"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }
}
