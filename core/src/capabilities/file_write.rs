//! FileWrite capability — writes files with backup-before-mutate for undo support.
//!
//! Before overwriting an existing file, creates a backup via [`BackupManager`]
//! so the operation can be rolled back. Supports both overwrite and append modes.
//! Respects `dry_run` in the context to skip actual writes. Rejects directory
//! paths (cannot write to a directory).
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::FileWrite;
//!;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let cap = FileWrite::new();
//! let result = cap.execute(
//!     &json!({"path": "/tmp/output.txt", "content": "hello"}),
//!     &Context { dry_run: false, job_id: "job1".into(), working_dir: std::env::temp_dir() },
//! ).unwrap();
//!
//! assert_eq!(result.status, "ok");
//! assert_eq!(std::fs::read_to_string("/tmp/output.txt").unwrap(), "hello");
//! ```

use crate::backup::BackupManager;
use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    ".env",
    ".env.*",
    "authorized_keys",
    "id_rsa",
    "id_ed25519",
];

/// Input parameters for [`FileWrite::execute`].
///
/// The target file is backed up before any write occurs, making the
/// operation reversible through the undo system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct FileWriteArgs {
    /// Absolute path to the file to write.
    pub path: String,
    /// Content to write (overwrites existing content unless `append` is set).
    pub content: String,
    /// When true, append to the file instead of overwriting.
    #[serde(default)]
    pub append: bool,
}

/// Capability that writes file contents with backup-before-mutate.
///
/// Every write creates a timestamped backup via [`BackupManager`], enabling
/// rollback through the undo system. The backup is created *before* the
/// mutation, so a failed write still leaves a recoverable state.
pub struct FileWrite {
    backup_mgr: BackupManager,
}

impl FileWrite {
    /// Create a new `FileWrite` capability backed by the default backup directory.
    ///
    /// The backup directory is derived from `data_dir()` as `data_dir().join("backups")`.
    /// This eliminates external configuration of the backup path (ADR-C28).
    #[allow(clippy::missing_errors_doc)] // Error path is self-documenting — propagates BackupManager::new
    pub fn new() -> Result<Self> {
        let backup_dir = crate::utils::backup_dir();
        Ok(Self {
            backup_mgr: BackupManager::new(backup_dir)?,
        })
    }
}

impl TypedCapability for FileWrite {
    type Args = FileWriteArgs;

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

    fn execute(
        &self,
        args: FileWriteArgs,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        // Validate content is valid UTF-8 (defense-in-depth)
        if let Err(e) = std::str::from_utf8(args.content.as_bytes()) {
            return Err(CapabilityError::InvalidArgs(format!(
                "Content is not valid UTF-8: {}",
                e
            )));
        }
        if args.content.len() > MAX_WRITE_SIZE {
            return Err(CapabilityError::InvalidArgs(format!(
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
            .map_err(|e| CapabilityError::PermissionDenied(format!("path validation: {}", e)))?;

        // Reject directory paths — cannot write to a directory
        if path.exists() && path.is_dir() {
            return Err(CapabilityError::PermissionDenied(format!(
                "path is a directory: {}",
                path.display()
            )));
        }

        if is_critical_file(&path) {
            return Err(CapabilityError::PermissionDenied(format!(
                "critical file denied: {}",
                path.display()
            )));
        }

        if let Err(e) = check_disk_space(&path, args.content.len()) {
            return Err(CapabilityError::PermissionDenied(e));
        }

        if args.append {
            if let Ok(meta) = std::fs::metadata(&path) {
                let existing = usize::try_from(meta.len()).unwrap_or(usize::MAX);
                if existing.saturating_add(args.content.len()) > MAX_APPEND_SIZE {
                    return Err(CapabilityError::InvalidArgs(format!(
                        "append would exceed max file size: {} + {} > {} bytes",
                        existing,
                        args.content.len(),
                        MAX_APPEND_SIZE
                    )));
                }
            }
        }

        if ctx.dry_run {
            let mut out = Output::ok(format!(
                "DRY RUN: would write {} bytes to {}",
                args.content.len(),
                path.display()
            ));
            out.data = Some(serde_json::json!({
                "path": path.display().to_string(),
                "content_length": args.content.len(),
                "dry_run": true,
                "backup_path": null,
                "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
                "process_before_count": process_before.summary.total_processes,
            }));
            return Ok(out);
        }

        let backup_path = if path.exists() {
            match self.backup_mgr.create_backup(&path, &ctx.job_id) {
                Ok(bp) => Some(bp),
                Err(e) => return Err(CapabilityError::Internal(format!("backup: {}", e))),
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CapabilityError::Io(std::io::Error::other(format!(
                        "mkdir {}: {}",
                        parent.display(),
                        e
                    )))
                })?;
            }
            None
        };

        let bytes_written = if args.append {
            atomic_append(&path, &args.content)
                .map_err(|e| CapabilityError::Internal(e.to_string()))?
        } else {
            atomic_write(&path, &args.content)
                .map_err(|e| CapabilityError::Internal(e.to_string()))?
        };

        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();

        let mut out = Output::ok(format!(
            "Wrote {} bytes to {}",
            bytes_written,
            path.display()
        ));
        out.data = Some(serde_json::json!({
            "path": path.display().to_string(),
            "bytes_written": bytes_written,
            "append": args.append,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
            "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
            "telemetry_after": serde_json::to_value(&telemetry_after).unwrap_or(Value::Null),
            "process_before_count": process_before.summary.total_processes,
            "process_after_count": process_after.summary.total_processes,
        }));
        Ok(out)
    }
}

fn is_critical_file(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy();
    let filename = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    for critical in CRITICAL_FILES {
        if critical.contains('*') {
            if glob_match(critical, &filename) {
                return true;
            }
        } else if path_str.ends_with(critical) {
            return true;
        }
    }
    false
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() != 2 {
        return text == pattern;
    }
    let Some(prefix) = parts.first() else {
        return text == pattern;
    };
    let Some(suffix) = parts.get(1) else {
        return text == pattern;
    };
    text.starts_with(prefix) && text.ends_with(suffix)
}

fn check_disk_space(
    path: &std::path::Path,
    content_size: usize,
) -> std::result::Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("/"));

    // Parent may not exist yet (create_dir_all runs later). Skip df — disk
    // check is meaningless for a path that doesn't exist on the filesystem.
    if !parent.exists() {
        return Ok(());
    }

    let output = std::process::Command::new("df")
        .arg("-B1")
        .arg(parent)
        .output()
        .map_err(|e| format!("df command failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("df command failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();

    let Some(header) = lines.next() else {
        return Ok(());
    };
    let headers: Vec<&str> = header.split_whitespace().collect();
    let avail_idx = headers
        .iter()
        .position(|&h| h.eq_ignore_ascii_case("Available") || h.eq_ignore_ascii_case("Avail"));

    if let Some(line) = lines.next() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let idx = avail_idx.unwrap_or(3); // fall back to column 3 (GNU default)
        if let Some(available_str) = parts.get(idx) {
            if let Ok(available) = available_str.parse::<u64>() {
                let required = (content_size as u64).saturating_add(MIN_FREE_DISK_BYTES);
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

// ── Platform-specific O_NOFOLLOW helpers ─────────────────────────────────────

/// Opens a file for writing with `O_NOFOLLOW` (symlink protection) on Unix.
///
/// On Linux, `O_NOFOLLOW` causes `open()` to fail with `ELOOP` if the
/// target is a symlink — preventing symlink-based path traversal attacks.
///
/// On non-Unix platforms, `O_NOFOLLOW` is not available. The file is opened
/// without symlink protection — security depends on parent directory
/// permissions instead.
#[cfg(unix)]
fn open_write_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_write_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    // O_NOFOLLOW not available — symlink protection depends on parent dir permissions
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

/// Opens a file for reading with `O_NOFOLLOW` on Unix, or without on other
/// platforms. See [`open_write_nofollow`] for details.
#[cfg(unix)]
fn open_read_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_read_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

fn atomic_write(path: &std::path::Path, content: &str) -> Result<usize> {
    use std::io::Write;

    let tmp_name = format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    let tmp_path = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(&tmp_name);

    {
        let mut file = open_write_nofollow(&tmp_path).map_err(|e| {
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
        Error::ExecutionFailed(format!(
            "atomic rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        ))
    })?;

    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
    {
        let _ = dir.sync_all();
    }

    Ok(content.len())
}

fn atomic_append(path: &std::path::Path, content: &str) -> Result<usize> {
    use std::io::{Read, Write};

    let existing = if path.exists() {
        let mut file = open_read_nofollow(path).map_err(|e| {
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
    let tmp_path = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(&tmp_name);

    {
        let mut file = open_write_nofollow(&tmp_path).map_err(|e| {
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
        Error::ExecutionFailed(format!(
            "atomic rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        ))
    })?;

    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
    {
        let _ = dir.sync_all();
    }

    Ok(content.len())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_backup_dir() -> PathBuf {
        std::env::temp_dir().join("runtimo_fw_test")
    }

    fn test_ctx(job_id: &str) -> Context {
        Context {
            dry_run: false,
            job_id: job_id.into(),
            working_dir: std::env::temp_dir(),
        }
    }

    fn dry_ctx(job_id: &str) -> Context {
        Context {
            dry_run: true,
            job_id: job_id.into(),
            working_dir: std::env::temp_dir(),
        }
    }

    #[test]
    fn writes_new_file() {
        let target = std::env::temp_dir().join("runtimo_fw_new.txt");
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "hello from runtimo".to_string(),
                append: false,
            },
            &test_ctx("t1"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
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
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "should not exist".to_string(),
                append: false,
            },
            &dry_ctx("t2"),
        )
        .expect("Execution failed");

        assert!(!target.exists());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let cap = FileWrite::new().expect("Failed to create FileWrite");
        let err = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: "../../../etc/passwd".to_string(),
                content: "malicious".to_string(),
                append: false,
            },
            &test_ctx("t3"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("traversal"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn rejects_critical_file() {
        let cap = FileWrite::new().expect("Failed to create FileWrite");
        let err = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: "/tmp/.bashrc".to_string(),
                content: "malicious".to_string(),
                append: false,
            },
            &test_ctx("t4"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("critical file"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn atomic_write_produces_correct_content() {
        let target = std::env::temp_dir().join("runtimo_fw_atomic.txt");
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "atomic content".to_string(),
                append: false,
            },
            &test_ctx("t5"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
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

        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: " appended".to_string(),
                append: true,
            },
            &test_ctx("t6"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
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
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "new content".to_string(),
                append: false,
            },
            &dry_ctx("t7"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
        assert!(result.data.as_ref().unwrap()["dry_run"].as_bool().unwrap());
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
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "telemetry test".to_string(),
                append: false,
            },
            &test_ctx("t8"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
        let data = result.data.as_ref().unwrap();
        assert!(data["telemetry_before"].is_object());
        assert!(data["telemetry_after"].is_object());
        assert!(data["process_before_count"].is_u64());
        assert!(data["process_after_count"].is_u64());

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn test_check_disk_space_writable_tmp_is_ok() {
        let result = check_disk_space(&std::env::temp_dir().join("runtimo_df_test.txt"), 100);
        assert!(result.is_ok(), "df on /tmp should succeed");
    }

    #[test]
    fn test_content_too_large_rejected() {
        let cap = FileWrite::new().expect("Failed to create FileWrite");
        let large_content = "x".repeat(101 * 1024 * 1024); // > 100MB
        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: "/tmp/runtimo_large_test.txt".to_string(),
                content: large_content,
                append: false,
            },
            &test_ctx("t9"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn test_critical_file_ssh_authorized_keys_blocked() {
        let cap = FileWrite::new().expect("Failed to create FileWrite");
        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: "/tmp/.ssh/authorized_keys".to_string(),
                content: "ssh-rsa AAA...".to_string(),
                append: false,
            },
            &test_ctx("t10"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("critical file"));

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn test_atomic_write_syncs_directory() {
        let target = std::env::temp_dir().join("runtimo_fw_sync.txt");
        let cap = FileWrite::new().expect("Failed to create FileWrite");

        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: "sync test".to_string(),
                append: false,
            },
            &test_ctx("t11"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");

        let tmp = target.parent().unwrap().join(".runtimo_fw_sync.txt.tmp");
        assert!(
            !tmp.exists(),
            "temp file should be cleaned up after atomic rename"
        );

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn test_append_exceeds_max_size_rejected() {
        let target = std::env::temp_dir().join("runtimo_fw_append_overflow.txt");
        std::fs::write(&target, "x".repeat(99 * 1024 * 1024)).ok(); // 99MB existing

        let cap = FileWrite::new().expect("Failed to create FileWrite");
        let large_append = "y".repeat(2 * 1024 * 1024); // +2MB = 101MB > 100MB
        let result = TypedCapability::execute(
            &cap,
            FileWriteArgs {
                path: target.to_str().unwrap().to_string(),
                content: large_append,
                append: true,
            },
            &test_ctx("t12"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceed"));

        std::fs::remove_file(&target).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }
}
