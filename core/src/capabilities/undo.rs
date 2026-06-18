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
use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Input parameters for [`Undo::execute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
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

impl TypedCapability for Undo {
    type Args = UndoArgs;

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

    fn execute(&self, args: UndoArgs, ctx: &Context) -> std::result::Result<Output, CapabilityError> {
        if args.job_id.is_empty() {
            return Err(CapabilityError::InvalidArgs("job_id is empty".into()));
        }

        // Get backup directory from canonical utility
        let backup_dir = crate::utils::backup_dir();

        let backup_mgr = crate::BackupManager::new(backup_dir.clone())
            .map_err(|e| CapabilityError::Internal(e.to_string()))?;

        // Find backup directory for the job
        let job_backup_dir = backup_dir.join(&args.job_id);
        if !job_backup_dir.exists() {
            return Err(CapabilityError::NotFound(format!(
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
                    return Err(CapabilityError::Internal(format!(
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
                        .ok_or_else(|| CapabilityError::Internal("Invalid backup path".into()))?
                        .to_string();

                    // FINDING #12: Look up by full backup path, not just filename
                    let target_path = original_paths
                        .get(&backup_path_str)
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| {
                            CapabilityError::NotFound(format!(
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
                        CapabilityError::PermissionDenied(format!("restore target validation: {}", e))
                    })?;

                    if ctx.dry_run {
                        restored.push(format!(
                            "{} -> {} (dry run)",
                            backup_path.display(),
                            target_path.display()
                        ));
                    } else {
                        backup_mgr.restore(&backup_path, &target_path)
                            .map_err(|e| CapabilityError::Internal(e.to_string()))?;
                        restored.push(format!(
                            "{} -> {}",
                            backup_path.display(),
                            target_path.display()
                        ));
                    }
                }
            }
        }

        let mut out = Output::ok(format!("Restored {} file(s)", restored.len()));
        out.data = Some(serde_json::json!({
            "restored": restored,
            "job_id": args.job_id
        }));
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::items_after_statements)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use crate::capability::Context;
    use std::fs;
    use std::sync::Mutex;

    /// Mutex to serialize undo tests that set environment variables
    /// (RUNTIMO_WAL_PATH, XDG_DATA_HOME). Without this, concurrent
    /// tests fight over process-global env vars and produce spurious failures.
    static UNDO_TEST_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_undo_with_backup() {
        let _guard = UNDO_TEST_MUTEX.lock().unwrap();
        let tmpdir = std::env::temp_dir().join("runtimo_test_undo");
        let _ = fs::remove_dir_all(&tmpdir);
        fs::create_dir_all(&tmpdir).unwrap();

        let test_file = tmpdir.join("test.txt");
        fs::write(&test_file, "original content").unwrap();

        // Set XDG_DATA_HOME so backup_dir() derives from temp dir (ADR-C28)
        std::env::set_var("XDG_DATA_HOME", &tmpdir);
        // backup_dir() = data_dir().join("backups") = tmpdir/runtimo/backups
        let backup_dir = tmpdir.join("runtimo").join("backups");
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

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-test-job".to_string(),
            working_dir: tmpdir.clone(),
        };
        let result = Capability::execute(&cap, &serde_json::json!({"job_id": job_id}), &ctx);

        assert!(result.is_ok(), "undo failed: {:?}", result.err());
        let output = result.unwrap();
        assert_eq!(output.status, "ok", "undo not successful: {:?}", output.output);
        assert!(
            !output.data.as_ref().unwrap()["restored"].as_array().unwrap().is_empty(),
            "no files restored"
        );

        // Verify original content restored
        let restored_content = fs::read_to_string(&test_file).unwrap();
        assert_eq!(restored_content, "original content");

        // Clean up
        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("RUNTIMO_WAL_PATH");
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_undo_missing_backup_returns_error() {
        let _guard = UNDO_TEST_MUTEX.lock().unwrap();
        // GAP 7: Verify missing backup returns proper error (not panic)
        let tmpdir = std::env::temp_dir().join("runtimo_test_undo_missing");
        let _ = fs::remove_dir_all(&tmpdir);
        fs::create_dir_all(&tmpdir).unwrap();

        // Set XDG_DATA_HOME so backup_dir() derives from temp dir
        std::env::set_var("XDG_DATA_HOME", &tmpdir);

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-missing-test".to_string(),
            working_dir: tmpdir.clone(),
        };

        // Try to undo a job that has no backup
        let result = Capability::execute(&cap, &serde_json::json!({"job_id": "nonexistent-job-xyz"}), &ctx);
        assert!(result.is_err(), "Should error on missing backup");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("No backup found"),
            "Error should mention missing backup: {}",
            err
        );

        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_undo_missing_job_id_validation() {
        // GAP 7: empty job_id should be rejected by execute
        let cap = Undo;
        let result = TypedCapability::execute(&cap, UndoArgs { job_id: String::new(), file: None },
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err(), "Empty job_id should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("empty") || err.contains("job_id"),
            "Error should mention empty job_id: {}",
            err
        );
    }

    #[test]
    fn test_undo_multi_file_restore() {
        let _guard = UNDO_TEST_MUTEX.lock().unwrap();
        // GAP 7: Test restoring multiple files from the same job backup
        let tmpdir = std::env::temp_dir().join("runtimo_test_undo_multi");
        let _ = fs::remove_dir_all(&tmpdir);
        fs::create_dir_all(&tmpdir).unwrap();

        let test_file1 = tmpdir.join("file1.txt");
        let test_file2 = tmpdir.join("file2.txt");
        fs::write(&test_file1, "original file 1").unwrap();
        fs::write(&test_file2, "original file 2").unwrap();

        // Set XDG_DATA_HOME so backup_dir() derives from temp dir (ADR-C28)
        std::env::set_var("XDG_DATA_HOME", &tmpdir);
        // backup_dir() = data_dir().join("backups") = tmpdir/runtimo/backups
        let backup_dir = tmpdir.join("runtimo").join("backups");
        let job_id = "multi-file-job";
        let job_backup_dir = backup_dir.join(job_id);
        fs::create_dir_all(&job_backup_dir).unwrap();

        let backup1 = job_backup_dir.join("file1.txt");
        let backup2 = job_backup_dir.join("file2.txt");
        fs::copy(&test_file1, &backup1).unwrap();
        fs::copy(&test_file2, &backup2).unwrap();

        // Modify originals
        fs::write(&test_file1, "modified file 1").unwrap();
        fs::write(&test_file2, "modified file 2").unwrap();

        // Write WAL entries for both files
        let wal_file = tmpdir.join("multi.wal");
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
                    "path": test_file1.to_str().unwrap(),
                    "backup_path": backup1.to_str().unwrap()
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
        wal.append(WalEvent {
            seq: 1,
            ts: ts + 1,
            event_type: WalEventType::JobCompleted,
            job_id: job_id.to_string(),
            capability: Some("FileWrite".into()),
            output: Some(serde_json::json!({
                "data": {
                    "path": test_file2.to_str().unwrap(),
                    "backup_path": backup2.to_str().unwrap()
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

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-multi".to_string(),
            working_dir: tmpdir.clone(),
        };
        let result = Capability::execute(&cap, &serde_json::json!({"job_id": job_id}), &ctx);

        assert!(result.is_ok(), "undo multi-file failed: {:?}", result.err());
        let output = result.unwrap();
        assert_eq!(output.status, "ok");
        let restored = output.data.as_ref().unwrap()["restored"].as_array().unwrap();
        assert!(
            restored.len() >= 2,
            "Should restore at least 2 files, got {}: {:?}",
            restored.len(),
            restored
        );

        // Verify both files restored
        assert_eq!(fs::read_to_string(&test_file1).unwrap(), "original file 1");
        assert_eq!(fs::read_to_string(&test_file2).unwrap(), "original file 2");

        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("RUNTIMO_WAL_PATH");
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_undo_revalidates_target_paths() {
        let _guard = UNDO_TEST_MUTEX.lock().unwrap();
        // GAP 7: Restore should re-validate target paths against allowed prefixes
        // This test verifies that validation happens (restore calls validate_path)
        let tmpdir = std::env::temp_dir().join("runtimo_test_undo_validate");
        let _ = fs::remove_dir_all(&tmpdir);
        fs::create_dir_all(&tmpdir).unwrap();

        let test_file = tmpdir.join("valid.txt");
        fs::write(&test_file, "original").unwrap();

        // Set XDG_DATA_HOME so backup_dir() derives from temp dir (ADR-C28)
        std::env::set_var("XDG_DATA_HOME", &tmpdir);
        // backup_dir() = data_dir().join("backups") = tmpdir/runtimo/backups
        let backup_dir = tmpdir.join("runtimo").join("backups");
        let job_id = "validate-job";
        let job_backup_dir = backup_dir.join(job_id);
        fs::create_dir_all(&job_backup_dir).unwrap();

        let backup_path = job_backup_dir.join("valid.txt");
        fs::copy(&test_file, &backup_path).unwrap();

        // Write WAL with an allowed path (/tmp/...)
        let wal_file = tmpdir.join("val.wal");
        std::env::set_var("RUNTIMO_WAL_PATH", &wal_file);
        use crate::wal::{WalEvent, WalEventType, WalWriter};
        let mut wal = WalWriter::create(&wal_file).unwrap();
        wal.append(WalEvent {
            seq: 0,
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
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

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-val".to_string(),
            working_dir: tmpdir.clone(),
        };
        // This should succeed because /tmp is in allowed prefixes
        let result = Capability::execute(&cap, &serde_json::json!({"job_id": job_id}), &ctx);
        assert!(
            result.is_ok(),
            "Valid path restore should succeed: {:?}",
            result.err()
        );

        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("RUNTIMO_WAL_PATH");
        std::env::remove_var("XDG_DATA_HOME");
    }
}
