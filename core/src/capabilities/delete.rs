//! Delete capability — removes a file with backup-before-delete for undo support.
//!
//! Deleting files through `rm` in ShellExec is intentionally hard-blocked;
//! this capability is the audited, path-validated alternative. Every delete
//! creates a backup via [`BackupManager`] (restorable via `Undo`), rejects
//! critical files, and validates the path against the allowed-prefix whitelist
//! before touching the filesystem. Directories are rejected — only regular
//! files can be deleted.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::Delete;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let cap = Delete::new().unwrap();
//! let result = cap.execute(
//!     &json!({"path": "/tmp/stale.lock"}),
//!     &Context { dry_run: false, job_id: "job1".into(), working_dir: std::env::temp_dir() },
//! ).unwrap();
//!
//! assert_eq!(result.status, "ok");
//! ```

use crate::backup::BackupManager;
use crate::capabilities::file_write::is_critical_file;
use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use crate::validation::path::{validate_path, PathContext};
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Input parameters for [`Delete::execute`].
///
/// The target file is backed up before deletion, making the operation
/// reversible through the undo system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct DeleteArgs {
    /// Absolute path to the file to delete.
    pub path: String,

    /// Skip the backup-before-delete (default: `false`).
    ///
    /// When `true`, the file is deleted without creating an undo backup.
    /// Intended for large files (hundreds of GB of models) where copying the
    /// target to `data_dir()/backups` would double disk usage under pressure.
    /// The operation is then irreversible.
    #[serde(default)]
    pub no_backup: bool,
}

/// Capability that deletes a file with backup-before-delete.
///
/// The backup is created *before* the file is removed, so a failed delete
/// still leaves a recoverable state, and `Undo` can restore the file by job.
pub struct Delete {
    backup_mgr: BackupManager,
}

impl Delete {
    /// Create a new `Delete` capability backed by the default backup directory.
    ///
    /// The backup directory is derived from `data_dir()` as
    /// `data_dir().join("backups")` — no external configuration (ADR-C28).
    #[allow(clippy::missing_errors_doc)] // Error path is self-documenting — propagates BackupManager::new
    pub fn new() -> Result<Self> {
        let backup_dir = crate::utils::backup_dir();
        Ok(Self {
            backup_mgr: BackupManager::new(backup_dir)?,
        })
    }
}

impl TypedCapability for Delete {
    type Args = DeleteArgs;

    fn name(&self) -> &'static str {
        "Delete"
    }

    fn description(&self) -> &'static str {
        "delete file. auto-backup for undo unless no_backup. path-validated (no rm bypass)."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "no_backup": { "type": "boolean", "default": false, "description": "Skip backup-before-delete (irreversible)" }
            },
            "required": ["path"]
        })
    }

    fn execute(
        &self,
        args: DeleteArgs,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        let telemetry_before = Telemetry::capture();
        let process_before = ProcessSnapshot::capture();

        // Existing regular file under an allowed prefix — full whitelist check.
        let delete_ctx = PathContext {
            require_exists: true,
            require_file: true,
            ..Default::default()
        };
        let path = validate_path(&args.path, &delete_ctx)
            .map_err(|e| CapabilityError::PermissionDenied(format!("path validation: {}", e)))?;

        if crate::config::RuntimoConfig::critical_files_enabled() && is_critical_file(&path) {
            return Err(CapabilityError::PermissionDenied(format!(
                "critical file denied: {}",
                path.display()
            )));
        }

        if ctx.dry_run {
            let mut out = Output::ok(format!("DRY RUN: would delete {}", path.display()));
            out.data = Some(serde_json::json!({
                "path": path.display().to_string(),
                "dry_run": true,
                "no_backup": args.no_backup,
                "backup_path": null,
                "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
                "process_before_count": process_before.summary.total_processes,
            }));
            return Ok(out);
        }

        // Backup unless explicitly skipped (no_backup) — large-file deletion
        // under disk pressure is the opt-out case.
        let backup_path: Option<std::path::PathBuf> = if args.no_backup {
            None
        } else {
            Some(
                self.backup_mgr
                    .create_backup(&path, &ctx.job_id)
                    .map_err(|e| CapabilityError::Internal(format!("backup: {}", e)))?,
            )
        };

        std::fs::remove_file(&path).map_err(|e| {
            CapabilityError::Io(std::io::Error::other(format!(
                "remove {}: {}",
                path.display(),
                e
            )))
        })?;

        // Durability: sync the parent directory so the unlink is not lost
        // on crash after WAL/telemetry processing.
        // Fragile sentinel: sync_all failure is silently discarded
        // (let _ = dir.sync_all()) — the unlink has already succeeded,
        // and failing the entire operation over a missing fsync would
        // be worse than accepting the durability risk.
        if let Ok(dir) =
            std::fs::File::open(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
        {
            let _ = dir.sync_all();
        }

        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();

        let mut out = Output::ok(format!("Deleted {}", path.display()));
        out.data = Some(serde_json::json!({
            "path": path.display().to_string(),
            "backup_path": if let Some(ref bp) = backup_path {
                Value::String(bp.to_string_lossy().to_string())
            } else {
                Value::Null
            },
            "no_backup": args.no_backup,
            "telemetry_before": serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
            "telemetry_after": serde_json::to_value(&telemetry_after).unwrap_or(Value::Null),
            "process_before_count": process_before.summary.total_processes,
            "process_after_count": process_after.summary.total_processes,
        }));
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    fn deletes_file_within_allowed_prefix() {
        let backup_dir = crate::utils::backup_dir();
        std::fs::create_dir_all(&backup_dir).ok();
        let target = std::env::temp_dir().join("runtimo_del_ok.txt");
        std::fs::write(&target, "stale").unwrap();
        let cap = Delete::new().unwrap();

        let result = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: target.to_str().unwrap().to_string(),
                no_backup: false,
            },
            &test_ctx("d1"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
        assert!(!target.exists(), "file should be deleted");
        assert!(
            result.data.as_ref().unwrap()["backup_path"]
                .as_str()
                .is_some(),
            "delete must create a backup for undo"
        );
    }

    #[test]
    fn rejects_traversal() {
        let cap = Delete::new().unwrap();
        let err = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: "../../../etc/passwd".to_string(),
                no_backup: false,
            },
            &test_ctx("d2"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("traversal"));
    }

    #[test]
    fn rejects_outside_allowed_prefix() {
        let cap = Delete::new().unwrap();
        let err = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: "/etc/shadow".to_string(),
                no_backup: false,
            },
            &test_ctx("d3"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("blocked"), "got: {}", err);
    }

    #[test]
    fn rejects_missing_file() {
        let cap = Delete::new().unwrap();
        let missing = std::env::temp_dir().join("runtimo_del_missing.txt");
        let err = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: missing.to_str().unwrap().to_string(),
                no_backup: false,
            },
            &test_ctx("d4"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not found") || err.to_string().contains("does not exist")
        );
    }

    #[test]
    fn rejects_directory() {
        let dir = std::env::temp_dir().join("runtimo_del_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let cap = Delete::new().unwrap();
        let err = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: dir.to_str().unwrap().to_string(),
                no_backup: false,
            },
            &test_ctx("d5"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a file"), "got: {}", err);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_critical_file() {
        let target = std::env::temp_dir().join(".bashrc");
        std::fs::write(&target, "alias ll='ls -la'").unwrap();
        let cap = Delete::new().unwrap();
        let err = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: target.to_str().unwrap().to_string(),
                no_backup: false,
            },
            &test_ctx("d6"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("critical file"), "got: {}", err);
        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn dry_run_does_not_delete() {
        let target = std::env::temp_dir().join("runtimo_del_dry.txt");
        std::fs::write(&target, "keep me").unwrap();
        let cap = Delete::new().unwrap();

        let result = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: target.to_str().unwrap().to_string(),
                no_backup: false,
            },
            &dry_ctx("d7"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
        assert!(target.exists(), "dry run must not delete");
        assert!(result.data.as_ref().unwrap()["dry_run"].as_bool().unwrap());

        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn no_backup_skips_backup_creation() {
        let target = std::env::temp_dir().join("runtimo_del_nobak.bin");
        std::fs::write(&target, "big model bytes").unwrap();
        let cap = Delete::new().unwrap();

        let result = TypedCapability::execute(
            &cap,
            DeleteArgs {
                path: target.to_str().unwrap().to_string(),
                no_backup: true,
            },
            &test_ctx("d8"),
        )
        .expect("Execution failed");

        assert_eq!(result.status, "ok");
        assert!(!target.exists(), "file should be deleted");
        let backup_path = result.data.as_ref().unwrap()["backup_path"].clone();
        assert!(
            backup_path.is_null(),
            "no_backup must not create a backup, got: {}",
            backup_path
        );
        assert!(result.data.as_ref().unwrap()["no_backup"]
            .as_bool()
            .unwrap());
    }
}
