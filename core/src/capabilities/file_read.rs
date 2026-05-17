//! FileRead capability — reads file contents with safety validation.
//!
//! Rejects path traversal (`..`), empty paths, non-existent files, and
//! directories. Returns the file content along with byte count.
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

/// Maximum file size allowed for reading (10 MB).
/// Reduced from 100 MB to prevent OOM on persistent machines (FINDING #10).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Default max bytes to read when max_bytes is not specified (1 MB).
/// Prevents accidental large reads even for files under MAX_FILE_SIZE.
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// Arguments for the [`FileRead`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadArgs {
    /// Absolute or relative path to the file to read.
    pub path: String,
    /// Maximum bytes to read (default: 1 MB, max: 10 MB). FINDING #10.
    pub max_bytes: Option<u64>,
}

/// Capability that reads the contents of a file.
///
/// Validates that the path exists, is a file (not a directory), and does not
/// contain `..` sequences (path traversal protection).
///
/// # Example
///
/// ```rust,ignore
/// use runtimo_core::capabilities::FileRead;
/// use runtimo_core::capability::{Capability, Context};
/// use serde_json::json;
///
/// // Create a test file first
/// std::fs::write("/tmp/hello.txt", "hello world").unwrap();
///
/// let cap = FileRead;
/// let result = cap.execute(
///     &json!({"path": "/tmp/hello.txt"}),
///     &Context { dry_run: false, job_id: "test".into() },
/// ).unwrap();
///
/// assert!(result.success);
/// assert_eq!(result.data["content"].as_str().unwrap(), "hello world\n");
/// ```
pub struct FileRead;

impl Capability for FileRead {
    fn name(&self) -> &'static str {
        "FileRead"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file. Validates path existence, rejects directories and path traversal."
    }

    /// Returns the JSON Schema for FileRead arguments.
    ///
    /// Schema: `{"type": "object", "properties": {"path": {"type": "string"}, "max_bytes": {"type": "integer"}}, "required": ["path"]}`
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

    /// Validates the path argument using unified validation module.
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

    /// Reads the file and returns its contents.
    ///
    /// Respects `max_bytes` parameter (default 1 MB, max 10 MB) to limit
    /// memory usage even for files under the hard MAX_FILE_SIZE cap.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ExecutionFailed`] if
    /// the file cannot be read (permissions, I/O error, etc.).
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

        let metadata = path
            .metadata()
            .map_err(|e| Error::ExecutionFailed(format!("stat {}: {}", path.display(), e)))?;

        if metadata.len() > MAX_FILE_SIZE {
            return Err(Error::ExecutionFailed(format!(
                "File too large: {} bytes (limit: {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            )));
        }

        // FINDING #10: Apply max_bytes limit (default 1 MB)
        let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        if max_bytes > MAX_FILE_SIZE {
            return Err(Error::ExecutionFailed(format!(
                "max_bytes {} exceeds maximum allowed {}",
                max_bytes, MAX_FILE_SIZE
            )));
        }

        let content = if metadata.len() <= max_bytes {
            std::fs::read_to_string(&path)
                .map_err(|e| Error::ExecutionFailed(format!("read {}: {}", path.display(), e)))?
        } else {
            // Read only up to max_bytes
            let file = std::fs::File::open(&path)
                .map_err(|e| Error::ExecutionFailed(format!("open {}: {}", path.display(), e)))?;
            use std::io::Read;
            let mut buf = String::new();
            let mut handle = file.take(max_bytes);
            handle
                .read_to_string(&mut buf)
                .map_err(|e| Error::ExecutionFailed(format!("read {}: {}", path.display(), e)))?;
            buf
        };

        let truncated = metadata.len() > max_bytes;

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "content": content,
                "path": path.display().to_string(),
                "bytes_read": content.len(),
                "file_size": metadata.len(),
                "truncated": truncated,
            }),
            message: Some(format!(
                "Read {} bytes from {}{}",
                content.len(),
                path.display(),
                if truncated { " (truncated)" } else { "" }
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        assert!(result.data["content"]
            .as_str()
            .unwrap()
            .contains("hello world"));
        std::fs::remove_file(&tmp).ok();
    }

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

    #[test]
    fn test_max_bytes_limits_output() {
        // FINDING #10: verify max_bytes parameter works
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
    // FINDING #10: Runtime validation rejects max_bytes > 10MB
    // Note: JSON Schema doesn't enforce maximum at parse time, but execute() checks it
    let result = FileRead.execute(
        &serde_json::json!({ "path": "/etc/hosts", "max_bytes": 9999999999u64 }),
        &Context {
            dry_run: false,
            job_id: "test".into(),
            working_dir: std::env::temp_dir(),
        },
    );
    // Execution should fail because max_bytes exceeds MAX_FILE_SIZE (10MB)
    assert!(result.is_err());
}

    #[test]
    fn test_file_read_default_max_bytes() {
        // Verify default max_bytes (1MB) is applied when not specified
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
}
