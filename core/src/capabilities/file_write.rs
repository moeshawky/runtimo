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
use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Maximum content size allowed for writing (100 MB).
const MAX_WRITE_SIZE: usize = 100 * 1024 * 1024;

/// Maximum cumulative file size for append mode (100 MB).
const MAX_APPEND_SIZE: usize = 100 * 1024 * 1024;

/// Minimum free disk space required before writing (10 MB).
const MIN_FREE_DISK_BYTES: u64 = 10 * 1024 * 1024;

/// Critical files that must never be modified by capabilities.
const CRITICAL_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".ssh/authorized_keys",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    ".ssh/config",
    ".vimrc",
    ".gitconfig",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "authorized_keys",
    "id_rsa",
    "id_ed25519",
];

/// Arguments for the [`FileWrite`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWriteArgs {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub append: bool,
}

/// Capability that writes file contents with backup-before-mutate.
pub struct FileWrite {
    backup_mgr: BackupManager,
}

impl FileWrite {
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
        "write file. auto-backup for undo. append ok."
    }

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

        let telemetry_before = Telemetry::capture();
        let process_before = ProcessSnapshot::capture();

        let write_ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };

        let path = validate_path(&args.path, &write_ctx)
            .map_err(|e| Error::ExecutionFailed(format!("path validation: {}", e)))?;

        if is_critical_file(&path) {
            return Err(Error::ExecutionFailed(format!(
                "critical file denied: {}",
                path.display()
            )));
        }

        if let Err(e) = check_disk_space(&path, args.content.len()) {
            return Err(Error::ExecutionFailed(e));
        }

        if args.append {
            if let Ok(meta) = std::fs::metadata(&path) {
                let existing = meta.len() as usize;
                if existing.saturating_add(args.content.len()) > MAX_APPEND_SIZE {
                    return Err(Error::ExecutionFailed(format!(
                        "append would exceed max file size: {} + {} > {} bytes",
                        existing,
                        args.content.len(),
                        MAX_APPEND_SIZE
                    )));
                }
            }
        }

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "path": path.display().to_string(),
                    "content_length": args.content.len(),
                    "dry_run": true,
                    "backup_path": null,
                    "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
                    "process_before_count": process_before.summary.total_processes,
                }),
                message: Some(format!(
                    "DRY RUN: would write {} bytes to {}",
                    args.content.len(),
                    path.display()
                )),
            });
        }

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

        let bytes_written = if args.append {
            atomic_append(&path, &args.content)?
        } else {
            atomic_write(&path, &args.content)?
        };

        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "path": path.display().to_string(),
                "bytes_written": bytes_written,
                "append": args.append,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
                "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
                "telemetry_after": serde_json::to_value(&telemetry_after).unwrap_or(Value::Null),
                "process_before_count": process_before.summary.total_processes,
                "process_after_count": process_after.summary.total_processes,
            }),
            message: Some(format!(
                "Wrote {} bytes to {}",
                bytes_written,
                path.display()
            )),
        })
    }
}

fn is_critical_file(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy();
    for critical in CRITICAL_FILES {
        if path_str.ends_with(critical) {
            return true;
        }
    }
    false
}

fn check_disk_space(path: &std::path::Path, content_size: usize) -> std::result::Result<(), String> {
    let parent = path.parent().unwrap_or(std::path::Path::new("/"));
    let output = std::process::Command::new("df")
        .arg("-B1")
        .arg(parent)
        .output()
        .map_err(|e| format!("df command failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().nth(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            if let Ok(available) = parts[3].parse::<u64>() {
                let required = content_size as u64 + MIN_FREE_DISK_BYTES;
                if available < required {
                    return Err(format!(
                        "insufficient disk space: {} bytes available, {} bytes required",
                        available, required
                    ));
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

fn atomic_write(path: &std::path::Path, content: &str) -> Result<usize> {
    use std::io::Write;

    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    let tmp_path = path.parent().unwrap_or(std::path::Path::new(".")).join(&tmp_name);

    {
        let mut file = std::fs::File::create(&tmp_path).map_err(|e| {
            Error::ExecutionFailed(format!("create temp {}: {}", tmp_path.display(), e))
        })?;
        file.write_all(content.as_bytes()).map_err(|e| {
            Error::ExecutionFailed(format!("write temp {}: {}", tmp_path.display(), e))
        })?;
        file.sync_all().map_err(|e| {
            Error::ExecutionFailed(format!("fsync temp {}: {}", tmp_path.display(), e))
        })?;
    }

    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        Error::ExecutionFailed(format!("atomic rename {} -> {}: {}", tmp_path.display(), path.display(), e))
    })?;

    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or(std::path::Path::new("."))) {
        let _ = dir.sync_all();
    }

    Ok(content.len())
}

fn atomic_append(path: &std::path::Path, content: &str) -> Result<usize> {
    use std::io::{Read, Write};

    let existing = if path.exists() {
        let mut file = std::fs::File::open(path).map_err(|e| {
            Error::ExecutionFailed(format!("open {} for append: {}", path.display(), e))
        })?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| {
            Error::ExecutionFailed(format!("read {} for append: {}", path.display(), e))
        })?;
        buf
    } else {
        Vec::new()
    };

    let mut combined = existing;
    combined.extend_from_slice(content.as_bytes());

    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    let tmp_path = path.parent().unwrap_or(std::path::Path::new(".")).join(&tmp_name);

    {
        let mut file = std::fs::File::create(&tmp_path).map_err(|e| {
            Error::ExecutionFailed(format!("create temp {}: {}", tmp_path.display(), e))
        })?;
        file.write_all(&combined).map_err(|e| {
            Error::ExecutionFailed(format!("write temp {}: {}", tmp_path.display(), e))
        })?;
        file.sync_all().map_err(|e| {
            Error::ExecutionFailed(format!("fsync temp {}: {}", tmp_path.display(), e))
        })?;
    }

    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        Error::ExecutionFailed(format!("atomic rename {} -> {}: {}", tmp_path.display(), path.display(), e))
    })?;

    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or(std::path::Path::new("."))) {
        let _ = dir.sync_all();
    }

    Ok(content.len())
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

    #[test]
    fn rejects_critical_file() {
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");
        let err = cap
            .execute(
                &serde_json::json!({
                    "path": "/tmp/.bashrc",
                    "content": "malicious"
                }),
                &Context {
                    dry_run: false,
                    job_id: "t3".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("critical file"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn atomic_write_produces_correct_content() {
        let target = std::env::temp_dir().join("runtimo_fw_atomic.txt");
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");

        let result = cap
            .execute(
                &serde_json::json!({
                    "path": target.to_str().unwrap(),
                    "content": "atomic content"
                }),
                &Context {
                    dry_run: false,
                    job_id: "t4".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "atomic content");
        let tmp = target.parent().unwrap().join(".runtimo_fw_atomic.txt.tmp");
        assert!(!tmp.exists(), "temp file should not remain");

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn append_mode_works() {
        let target = std::env::temp_dir().join("runtimo_fw_append.txt");
        std::fs::write(&target, "initial").ok();

        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");

        let result = cap
            .execute(
                &serde_json::json!({
                    "path": target.to_str().unwrap(),
                    "content": " appended",
                    "append": true
                }),
                &Context {
                    dry_run: false,
                    job_id: "t5".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "initial appended"
        );

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn dry_run_does_not_create_backup() {
        let target = std::env::temp_dir().join("runtimo_fw_dry_backup.txt");
        std::fs::write(&target, "existing content").ok();

        // Use unique backup dir to avoid pollution from parallel tests
        let backup_dir = std::env::temp_dir().join("runtimo_fw_dry_backup_test");
        let _ = std::fs::remove_dir_all(&backup_dir);
        let cap = FileWrite::new(backup_dir.clone()).expect("Failed to create FileWrite");

        let result = cap
            .execute(
                &serde_json::json!({
                    "path": target.to_str().unwrap(),
                    "content": "new content"
                }),
                &Context {
                    dry_run: true,
                    job_id: "t6".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["dry_run"].as_bool().unwrap());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing content"
        );
        if backup_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&backup_dir)
                .map(|d| d.filter_map(|e| e.ok()).collect::<Vec<_>>())
                .unwrap_or_default();
            assert!(entries.is_empty());
        }

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(&backup_dir).ok();
    }

    #[test]
    fn telemetry_included_in_output() {
        let target = std::env::temp_dir().join("runtimo_fw_telemetry.txt");
        let cap = FileWrite::new(test_backup_dir()).expect("Failed to create FileWrite");

        let result = cap
            .execute(
                &serde_json::json!({
                    "path": target.to_str().unwrap(),
                    "content": "telemetry test"
                }),
                &Context {
                    dry_run: false,
                    job_id: "t7".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["telemetry_before"].is_object());
        assert!(result.data["telemetry_after"].is_object());
        assert!(result.data["process_before_count"].is_u64());
        assert!(result.data["process_after_count"].is_u64());

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }
}
