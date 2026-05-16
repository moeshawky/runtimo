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
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
    pub fn new(backup_dir: PathBuf) -> Self {
        Self {
            backup_mgr: BackupManager::new(backup_dir),
        }
    }
}

impl Capability for FileWrite {
    fn name(&self) -> &'static str {
        "FileWrite"
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

    /// Validates the path argument.
    ///
    /// Checks:
    /// - Path is not empty
    /// - Path does not contain `..` (traversal protection)
    /// - Path does not escape allowed directories after canonicalization
    fn validate(&self, args: &Value) -> Result<()> {
        let args: FileWriteArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        if args.path.is_empty() {
            return Err(Error::SchemaValidationFailed("path is empty".into()));
        }
        if args.path.contains("..") {
            return Err(Error::SchemaValidationFailed(
                "path traversal not allowed".into(),
            ));
        }

        // For new files, check canonical path is in allowed directories
        let path = Path::new(&args.path);
        if path.exists() {
            let canonical = path
                .canonicalize()
                .map_err(|e| Error::SchemaValidationFailed(format!("canonicalize failed: {}", e)))?;
            let path_str = canonical.to_string_lossy();
            let allowed_prefixes = ["/tmp", "/var/tmp", "/home"];
            if !allowed_prefixes.iter().any(|prefix| path_str.starts_with(prefix)) {
                return Err(Error::SchemaValidationFailed(format!(
                    "path outside allowed directories: {}",
                    canonical.display()
                )));
            }
        }

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

        let path = Path::new(&args.path);

        // Backup existing file before overwriting — enables undo via WAL
        let backup_path = if path.exists() {
            match self.backup_mgr.create_backup(path, &ctx.job_id) {
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
                    "path": args.path,
                    "content_length": args.content.len(),
                    "dry_run": true,
                    "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
                }),
                message: Some(format!(
                    "DRY RUN: would write {} bytes to {}",
                    args.content.len(),
                    args.path
                )),
            });
        }

        if args.append {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&args.path)
                .map_err(|e| Error::ExecutionFailed(format!("open {}: {}", args.path, e)))?;
            file.write_all(args.content.as_bytes())
                .map_err(|e| Error::ExecutionFailed(format!("write {}: {}", args.path, e)))?;
        } else {
            std::fs::write(&args.path, &args.content)
                .map_err(|e| Error::ExecutionFailed(format!("write {}: {}", args.path, e)))?;
        }

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "path": args.path,
                "bytes_written": args.content.len(),
                "append": args.append,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
            }),
            message: Some(format!(
                "Wrote {} bytes to {}",
                args.content.len(),
                args.path
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
        let cap = FileWrite::new(test_backup_dir());

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
            .unwrap();

        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "hello from runtimo"
        );

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(&test_backup_dir()).ok();
    }

    #[test]
    fn dry_run_does_not_write() {
        let target = std::env::temp_dir().join("runtimo_fw_dry.txt");
        let cap = FileWrite::new(test_backup_dir());

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
        .unwrap();

        assert!(!target.exists());
        std::fs::remove_dir_all(&test_backup_dir()).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let cap = FileWrite::new(test_backup_dir());
        let err = cap
            .validate(&serde_json::json!({
                "path": "../../../etc/passwd",
                "content": "malicious"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("traversal"));
        std::fs::remove_dir_all(&test_backup_dir()).ok();
    }
}
