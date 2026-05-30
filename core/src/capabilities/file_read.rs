//! FileRead capability — reads file contents with safety validation.
//!
//! Rejects path traversal (`..`), empty paths, non-existent files, and
//! directories. Returns the file content along with byte count.
//!
//! Security: opens file with O_NOFOLLOW to prevent TOCTOU symlink escape,
//! uses bounded reader (take) regardless of metadata to prevent size bypass,
//! detects binary content, and handles UTF-8 boundary splits correctly.
//!
//! # Example
//!
//! ```rust
//! use runtimo_core::capabilities::FileRead;
//! use runtimo_core::capability::Capability;
//! use serde_json::json;
//!
//! let cap = FileRead;
//! assert_eq!(cap.name(), "FileRead");
//!
//! // Schema requires a "path" string:
//! let schema = cap.schema();
//! assert!(schema["required"].as_array().unwrap().contains(&json!("path")));
//! ```

use crate::capability::{Capability, Context, Output};
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;

/// Maximum file size allowed for reading (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Default max bytes to read when max_bytes is not specified (1 MB).
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// Arguments for the [`FileRead`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadArgs {
    /// Absolute or relative path to the file to read.
    pub path: String,
    /// Maximum bytes to read (default: 1 MB, max: 10 MB).
    pub max_bytes: Option<u64>,
}

/// Capability that reads the contents of a file.
///
/// Opens file with O_NOFOLLOW to prevent TOCTOU symlink escape,
/// uses bounded reader regardless of metadata to prevent size bypass,
/// detects binary content, and handles UTF-8 boundary splits.
#[allow(clippy::exhaustive_structs)] // unit struct used as trait-object marker
pub struct FileRead;

impl Capability for FileRead {
    fn name(&self) -> &'static str {
        "FileRead"
    }

    fn description(&self) -> &'static str {
        "read file. path validated. no dirs, no traversal."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "max_bytes": { "type": "integer", "minimum": 1, "maximum": 10485760 }
            },
            "required": ["path"]
        })
    }

    fn validate(&self, args: &Value) -> Result<()> {
        let args: FileReadArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        let ctx = PathContext {
            require_exists: true,
            require_file: true,
            ..Default::default()
        };

        validate_path(&args.path, &ctx).map_err(Error::SchemaValidationFailed)?;

        Ok(())
    }

    fn execute(&self, args: &Value, _ctx: &Context) -> Result<Output> {
        let args: FileReadArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        let ctx = PathContext {
            require_exists: true,
            require_file: true,
            ..Default::default()
        };

        let path = validate_path(&args.path, &ctx)
            .map_err(|e| Error::ExecutionFailed(format!("path validation: {}", e)))?;

        let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        if max_bytes > MAX_FILE_SIZE {
            return Err(Error::ExecutionFailed(format!(
                "max_bytes {} exceeds maximum allowed {}",
                max_bytes, MAX_FILE_SIZE
            )));
        }

        // P0 FIX: Open with O_NOFOLLOW to prevent TOCTOU symlink escape.
        // Open immediately after validation to minimize TOCTOU window.
        let file = open_file_nofollow(&path)
            .map_err(|e| Error::ExecutionFailed(format!("open {}: {}", path.display(), e)))?;

        // P0 FIX: Always use bounded reader (take) regardless of metadata.
        // Prevents TOCTOU size bypass where file grows between stat and read.
        let mut limited = file.take(max_bytes);

        // Read raw bytes to handle binary detection and UTF-8 boundaries correctly.
        let mut raw_bytes = Vec::with_capacity(
            std::cmp::min(usize::try_from(max_bytes).unwrap_or(usize::MAX), 64 * 1024)
        );
        let bytes_read = limited
            .read_to_end(&mut raw_bytes)
            .map_err(|e| Error::ExecutionFailed(format!("read {}: {}", path.display(), e)))?;

        let bytes_read = bytes_read as u64;
        let truncated = bytes_read >= max_bytes;

        // P1 FIX: Detect binary content (null bytes in the data).
        let is_binary = detect_binary(&raw_bytes);

        let data = if is_binary {
            serde_json::json!({
                "content_type": "binary",
                "path": path.display().to_string(),
                "bytes_read": bytes_read,
                "truncated": truncated,
                "message": "Binary file detected — content not returned as text",
            })
        } else {
            // P1 FIX: Convert raw bytes to String, trimming to valid UTF-8 boundary.
            let content = bytes_to_utf8_string(&raw_bytes);

            // P1 FIX: Parse JSON from slice (avoids double memory vs from_str).
            if path.extension().is_some_and(|ext| ext == "json") {
                match serde_json::from_slice::<Value>(raw_bytes.as_slice()) {
                    Ok(parsed) => serde_json::json!({
                        "content": parsed,
                        "content_type": "json",
                        "path": path.display().to_string(),
                        "bytes_read": bytes_read,
                        "truncated": truncated,
                    }),
                    Err(_) => serde_json::json!({
                        "content": content,
                        "content_type": "text",
                        "path": path.display().to_string(),
                        "bytes_read": bytes_read,
                        "truncated": truncated,
                    }),
                }
            } else {
                serde_json::json!({
                    "content": content,
                    "content_type": "text",
                    "path": path.display().to_string(),
                    "bytes_read": bytes_read,
                    "truncated": truncated,
                })
            }
        };

        Ok(Output {
            success: true,
            data,
            message: Some(format!(
                "Read {} bytes from {}{}",
                bytes_read,
                path.display(),
                if truncated { " (truncated)" } else { "" }
            )),
        })
    }
}

/// Open a file with O_NOFOLLOW to prevent TOCTOU symlink replacement attacks.
#[cfg(unix)]
fn open_file_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_file_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Detect binary content by checking for null bytes.
fn detect_binary(data: &[u8]) -> bool {
    data.contains(&0)
}

/// Convert raw bytes to a UTF-8 String, trimming trailing bytes that would
/// split a multibyte character boundary.
fn bytes_to_utf8_string(bytes: &[u8]) -> String {
    match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(e) => {
            let valid_up_to = e.utf8_error().valid_up_to();
            bytes.get(..valid_up_to)
                .map(|s| String::from_utf8(s.to_vec()).unwrap_or_default())
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[allow(clippy::unwrap_used, clippy::unused_result_ok)]
    #[test]
    fn reads_existing_file() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_read.txt");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            writeln!(f, "hello world").unwrap();
        }

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap() }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data.get("content")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("hello world"));
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn rejects_missing_file() {
        let err = FileRead
            .validate(&serde_json::json!({
                "path": "/tmp/nonexistent_runtimo_test.txt"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn rejects_empty_path() {
        assert!(FileRead
            .validate(&serde_json::json!({ "path": "" }))
            .is_err());
    }

    #[allow(clippy::indexing_slicing)]
    #[allow(clippy::unused_result_ok)]
    #[test]
    #[allow(clippy::unwrap_used, clippy::unused_result_ok)]
    fn test_max_bytes_limits_output() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_max_bytes.txt");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            for _ in 0..100 {
                writeln!(f, "hello world line").unwrap();
            }
        }

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap(), "max_bytes": 50 }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data["truncated"].as_bool() == Some(true));
        assert!(result.data["bytes_read"].as_u64().unwrap() <= 50);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_max_bytes_rejects_exceeding_limit() {
        let result = FileRead.execute(
            &serde_json::json!({ "path": "/etc/hosts", "max_bytes": 9999999999u64 }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err());
    }

    #[allow(clippy::indexing_slicing)]
    #[test]
    fn test_file_read_default_max_bytes() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_default_max.txt");
        std::fs::write(&tmp, "small content").unwrap();

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap() }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data["truncated"].as_bool() == Some(false));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn test_file_read_json_parsed_for_agents() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_agent.json");
        std::fs::write(&tmp, r#"{"key": "value", "nested": {"a": 1}}"#).unwrap();

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap() }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data["content"].is_object());
        assert_eq!(result.data["content"]["key"].as_str(), Some("value"));
        assert_eq!(result.data["content"]["nested"]["a"].as_u64(), Some(1));
        assert_eq!(result.data["content_type"].as_str(), Some("json"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_binary_file_detected() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_binary.bin");
        std::fs::write(&tmp, b"hello\x00world").unwrap();

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap() }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert_eq!(result.data["content_type"].as_str(), Some("binary"));
        assert_eq!(result.data["bytes_read"].as_u64(), Some(11));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_utf8_boundary_truncation() {
        // "café" = [99, 97, 102, 195, 169] — é is 2 bytes
        // Truncate at 4 bytes would split the é character
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_utf8.txt");
        std::fs::write(&tmp, b"caf\xc3\xa9").unwrap();

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap(), "max_bytes": 4 }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        let content = result.data["content"].as_str().unwrap();
        assert_eq!(content, "caf");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_bytes_read_reports_raw_bytes() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_bytes_read.txt");
        // UTF-8: "café\n" = 6 bytes (é is 2 bytes)
        std::fs::write(&tmp, "café\n").unwrap();

        let result = FileRead
            .execute(
                &serde_json::json!({ "path": tmp.to_str().unwrap() }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .unwrap();

        assert!(result.success);
        // bytes_read should be 6 (raw file bytes), not String::len() which is 5
        assert_eq!(result.data["bytes_read"].as_u64(), Some(6));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_symlink_rejected_by_nofollow() {
        let link_path = std::env::temp_dir().join("runtimo_nofollow_test");
        let _ = std::fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            if symlink("/etc/hostname", &link_path).is_ok() {
                let result = FileRead.execute(
                    &serde_json::json!({ "path": link_path.to_str().unwrap() }),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir(),
                    },
                );
                assert!(result.is_err(), "symlink should be rejected by O_NOFOLLOW");
                std::fs::remove_file(&link_path).ok();
            }
        }
    }
}
