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

/// Maximum file size allowed for reading (100 MB).
/// Prevents OOM on persistent machines when agents request multi-gigabyte files.
const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// Arguments for the [`FileRead`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadArgs {
    /// Absolute or relative path to the file to read.
    pub path: String,
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

    /// Returns the JSON Schema for FileRead arguments.
    ///
    /// Schema: `{"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}`
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
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

        let content = std::fs::read_to_string(&path)
            .map_err(|e| Error::ExecutionFailed(format!("read {}: {}", path.display(), e)))?;

        Ok(Output {
            success: true,
            data: serde_json::json!({ "content": content, "path": path.display().to_string() }),
            message: Some(format!(
                "Read {} bytes from {}",
                content.len(),
                path.display()
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
}
