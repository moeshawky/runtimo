//! Undo capability — restores files from backup.
//!
//! Enables rollback of file mutations by restoring from backups created
//! by [`FileWrite`](crate::capabilities::FileWrite).
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::{Undo, Capability};
//! use serde_json::json;
//!
//! let cap = Undo;
//! let result = cap.execute(
//!     &json!({"job_id": "abc123"}),
//!     &Context::default()
//! ).unwrap();
//! ```

use crate::validation::path::{validate_path, PathContext};
use crate::{Capability, Context, Output, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Input parameters for [`Undo::execute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoArgs {
    /// Job ID to undo (restores backup from that job).
    pub job_id: String,
    /// Optional: specific file to restore (if job modified multiple).
    pub file: Option<String>,
}

/// Capability that restores files from backup.
///
/// Each undo reverses the file mutations made by a specific job,
/// restoring the pre-mutation state captured by [`FileWrite`](crate::capabilities::FileWrite).
// This struct is intentionally simple — it implements the Capability trait
// and adding non-pub fields would serve no purpose.
#[allow(clippy::exhaustive_structs)]
pub struct Undo;

impl Capability for Undo {
    fn name(&self) -> &'static str {
        "Undo"
    }

    fn description(&self) -> &'static str {
        "restore from backup. use `runtimo logs` for job IDs."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" },
                "file": { "type": "string" }
            },
            "required": ["job_id"]
        })
    }

    fn validate(&self, args: &serde_json::Value) -> Result<()> {
        let args: UndoArgs = serde_json::from_value(args.clone())
            .map_err(|e| crate::Error::SchemaValidationFailed(e.to_string()))?;
        if args.job_id.is_empty() {
            return Err(crate::Error::SchemaValidationFailed(
                "job_id is empty".into(),
            ));
        }
        Ok(())
    }

    fn execute(&self, args: &serde_json::Value, ctx: &Context) -> Result<Output> {
        let args: UndoArgs = serde_json::from_value(args.clone())
            .map_err(|e| crate::Error::SchemaValidationFailed(e.to_string()))?;

        // Get backup directory from canonical utility
        let backup_dir = crate::utils::backup_dir();

        let backup_mgr = crate::BackupManager::new(backup_dir.clone())?;

        // Find backup directory for the job
        let job_backup_dir = backup_dir.join(&args.job_id);
        if !job_backup_dir.exists() {
            return Err(crate::Error::ExecutionFailed(format!(
                "No backup found for job {}",
                args.job_id
            )));
        }

        let mut restored = Vec::new();

        // Read WAL to find original paths for this job
        let wal_path = crate::utils::wal_path();

        let mut original_paths: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if wal_path.exists() {
            match crate::WalReader::load(&wal_path) {
                Ok(reader) => {
                    for event in reader.events() {
                        if event.job_id == args.job_id {
                            if let Some(output) = &event.output {
                                if let Some(data) = output.get("data") {
                                    if let Some(path) = data.get("path").and_then(|p| p.as_str()) {
                                        if let Some(backup) =
                                            data.get("backup_path").and_then(|b| b.as_str())
                                        {
                                            // FINDING #12: Use full backup path as key, not just filename
                                            // This prevents collisions when multiple files share the same name
                                            original_paths
                                                .insert(backup.to_string(), path.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(crate::Error::ExecutionFailed(format!(
                        "Failed to load WAL for undo: {}",
                        e
                    )));
                }
            }
        }

        // Restore all files in the job's backup directory
        if let Ok(entries) = std::fs::read_dir(&job_backup_dir) {
            for entry in entries.flatten() {
                let backup_path = entry.path();
                if backup_path.is_file() {
                    let backup_path_str = backup_path
                        .to_str()
                        .ok_or_else(|| crate::Error::ExecutionFailed("Invalid backup path".into()))?
                        .to_string();

                    // FINDING #12: Look up by full backup path, not just filename
                    let target_path = original_paths
                        .get(&backup_path_str)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            crate::Error::ExecutionFailed(format!(
                                "No original path found for backup {}",
                                backup_path.display()
                            ))
                        })?;

                    // Re-validate restore target against allowed prefixes
                    // (WAL data crossed a persistence boundary — must re-check)
                    let restore_ctx = PathContext {
                        require_exists: false,
                        require_file: false,
                        ..Default::default()
                    };
                    let target_path = validate_path(&target_path.to_string_lossy(), &restore_ctx)
                        .map_err(|e| {
                        crate::Error::ExecutionFailed(format!("restore target validation: {}", e))
                    })?;

                    if ctx.dry_run {
                        restored.push(format!(
                            "{} -> {} (dry run)",
                            backup_path.display(),
                            target_path.display()
                        ));
                    } else {
                        backup_mgr.restore(&backup_path, &target_path)?;
                        restored.push(format!(
                            "{} -> {}",
                            backup_path.display(),
                            target_path.display()
                        ));
                    }
                }
            }
        }

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "restored": restored,
                "job_id": args.job_id
            }),
            message: Some(format!("Restored {} file(s)", restored.len())),
        })
    }
}

#[cfg(test)]
#[allow(clippy::items_after_statements)]
mod tests {
    use super::*;
    use crate::capability::Context;
    use std::fs;

    #[test]
    fn test_undo_with_backup() {
        let tmpdir = std::env::temp_dir().join("runtimo_test_undo");
        let _ = fs::remove_dir_all(&tmpdir);
        fs::create_dir_all(&tmpdir).unwrap();

        let test_file = tmpdir.join("test.txt");
        fs::write(&test_file, "original content").unwrap();

        let backup_dir = tmpdir.join("backups");
        let job_id = "test-job-123";
        let job_backup_dir = backup_dir.join(job_id);
        fs::create_dir_all(&job_backup_dir).unwrap();

        let backup_path = job_backup_dir.join("test.txt");
        fs::copy(&test_file, &backup_path).unwrap();

        // Modify original
        fs::write(&test_file, "modified content").unwrap();

        // Write a WAL entry so undo can find the backup→original mapping
        let wal_file = tmpdir.join("test.wal");
        std::env::set_var("RUNTIMO_WAL_PATH", &wal_file);
        use crate::wal::{WalEvent, WalEventType, WalWriter};
        let mut wal = WalWriter::create(&wal_file).unwrap();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        wal.append(WalEvent {
            seq: 0,
            ts,
            event_type: WalEventType::JobCompleted,
            job_id: job_id.to_string(),
            capability: Some("FileWrite".into()),
            output: Some(serde_json::json!({
                "data": {
                    "path": test_file.to_str().unwrap(),
                    "backup_path": backup_path.to_str().unwrap()
                }
            })),
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
            cmd: None,
            cmd_stdout: None,
            cmd_stderr: None,
            cmd_exit_code: None,
            cmd_corrected: None,
            ..Default::default()
        })
        .unwrap();

        std::env::set_var("RUNTIMO_BACKUP_DIR", &backup_dir);

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-test-job".to_string(),
            working_dir: tmpdir.clone(),
        };
        let result = cap.execute(&serde_json::json!({"job_id": job_id}), &ctx);

        assert!(result.is_ok(), "undo failed: {:?}", result.err());
        let output = result.unwrap();
        assert!(output.success, "undo not successful: {:?}", output.message);
        assert!(
            !output.data["restored"].as_array().unwrap().is_empty(),
            "no files restored"
        );

        // Verify original content restored
        let restored_content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(restored_content, "original content");

        // Clean up
        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("RUNTIMO_WAL_PATH");
        std::env::remove_var("RUNTIMO_BACKUP_DIR");
    }
}
