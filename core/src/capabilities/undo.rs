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

use crate::{Capability, Context, Output, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoArgs {
    /// Job ID to undo (restores backup from that job).
    pub job_id: String,
    /// Optional: specific file to restore (if job modified multiple).
    pub file: Option<String>,
}

pub struct Undo;

impl Capability for Undo {
    fn name(&self) -> &'static str {
        "Undo"
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

    fn execute(&self, args: &serde_json::Value, _ctx: &Context) -> Result<Output> {
        let args: UndoArgs = serde_json::from_value(args.clone())
            .map_err(|e| crate::Error::SchemaValidationFailed(e.to_string()))?;

        // Get backup directory from environment or default
        let backup_dir = std::env::var("RUNTIMO_BACKUP_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                use std::path::PathBuf;
                std::env::var("XDG_DATA_HOME")
                    .ok()
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .map(|h| PathBuf::from(h).join(".local/share"))
                    })
                    .unwrap_or_else(std::env::temp_dir)
                    .join("runtimo")
                    .join("backups")
            });

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
        let wal_path = std::env::var("RUNTIMO_WAL_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                use std::path::PathBuf;
                std::env::var("XDG_DATA_HOME")
                    .ok()
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .map(|h| PathBuf::from(h).join(".local/share"))
                    })
                    .unwrap_or_else(std::env::temp_dir)
                    .join("runtimo")
                    .join("wal.jsonl")
            });

        let mut original_paths: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if wal_path.exists() {
            if let Ok(reader) = crate::WalReader::load(&wal_path) {
                for event in reader.events() {
                    if event.job_id == args.job_id {
                        if let Some(output) = &event.output {
                            // Extract path from FileWrite output (nested under "data")
                            if let Some(data) = output.get("data") {
                                if let Some(path) = data.get("path").and_then(|p| p.as_str()) {
                                    if let Some(backup) =
                                        data.get("backup_path").and_then(|b| b.as_str())
                                    {
                                        if let Some(filename) = std::path::Path::new(backup)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                        {
                                            original_paths
                                                .insert(filename.to_string(), path.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Restore all files in the job's backup directory
        if let Ok(entries) = std::fs::read_dir(&job_backup_dir) {
            for entry in entries.flatten() {
                let backup_path = entry.path();
                if backup_path.is_file() {
                    let filename = backup_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| {
                            crate::Error::ExecutionFailed("Invalid backup filename".into())
                        })?;

                    // Restore to original location if known, otherwise use filename
                    let target_path = original_paths
                        .get(filename)
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from(filename));

                    backup_mgr.restore(&backup_path, &target_path)?;
                    restored.push(format!("{} -> {}", filename, target_path.display()));
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

        // Create backup manually
        let backup_dir = tmpdir.join("backups");
        let job_id = "test-job-123";
        let job_backup_dir = backup_dir.join(job_id);
        fs::create_dir_all(&job_backup_dir).unwrap();

        let backup_path = job_backup_dir.join("test.txt");
        fs::copy(&test_file, &backup_path).unwrap();

        // Modify original
        fs::write(&test_file, "modified content").unwrap();

        // Set backup dir env
        std::env::set_var("RUNTIMO_BACKUP_DIR", &backup_dir);

        let cap = Undo;
        let ctx = Context {
            dry_run: false,
            job_id: "undo-test-job".to_string(),
            working_dir: tmpdir.clone(),
        };
        let result = cap.execute(&serde_json::json!({"job_id": job_id}), &ctx);

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.success);

        // Clean up
        let _ = fs::remove_dir_all(&tmpdir);
        std::env::remove_var("RUNTIMO_BACKUP_DIR");
    }
}
