//! FileRead capability — reads file contents with safety validation.
//!
//! Rejects path traversal (`..`), empty paths, non-existent files, and
//! directories. Returns the file content along with byte count.
//!
//! Security: opens file with O_NOFOLLOW to prevent TOCTOU symlink escape,
//! uses bounded reader (take) regardless of metadata to prevent size bypass,
//! detects binary content (null bytes or >10% control chars), and handles
//! UTF-8 boundary splits correctly.
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

use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::validation::path::{validate_path, PathContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;

/// Maximum file size allowed for reading (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Default max bytes to read when max_bytes is not specified (1 MB).
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// Input parameters for [`FileRead::execute`].
///
/// Accepts a file path and an optional byte limit. The path is validated
/// against the configured allowed-prefix list before any I/O occurs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct FileReadArgs {
    /// Absolute or relative path to the file to read.
    pub path: String,
    /// Maximum bytes to read (default: 1 MB, max: 10 MB, minimum: 1).
    pub max_bytes: Option<u64>,
}

/// Capability that reads the contents of a file.
///
/// Opens file with O_NOFOLLOW to prevent TOCTOU symlink escape,
/// uses bounded reader regardless of metadata to prevent size bypass,
/// detects binary content, and handles UTF-8 boundary splits.
#[allow(clippy::exhaustive_structs)] // unit struct used as trait-object marker
pub struct FileRead;

impl TypedCapability for FileRead {
    type Args = FileReadArgs;

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

    /// FileRead is side-effect-free; context (dry_run etc.) is ignored.
    fn execute(
        &self,
        args: FileReadArgs,
        _ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        let ctx = PathContext {
            require_exists: true,
            require_file: true,
            ..Default::default()
        };

        let path = validate_path(&args.path, &ctx)
            .map_err(|e| CapabilityError::PermissionDenied(format!("path validation: {}", e)))?;

        let max_bytes = args.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
        if max_bytes == 0 {
            return Err(CapabilityError::InvalidArgs(
                "max_bytes must be >= 1".into(),
            ));
        }
        if max_bytes > MAX_FILE_SIZE {
            return Err(CapabilityError::InvalidArgs(format!(
                "max_bytes {} exceeds maximum allowed {}",
                max_bytes, MAX_FILE_SIZE
            )));
        }

        // P0 FIX: Open with O_NOFOLLOW to prevent TOCTOU symlink escape.
        // Open immediately after validation to minimize TOCTOU window.
        let file = open_file_nofollow(&path).map_err(CapabilityError::Io)?;

        // P0 FIX: Always use bounded reader (take) regardless of metadata.
        // Prevents TOCTOU size bypass where file grows between stat and read.
        //
        // F-036: the cap is max_bytes + 1 so ONE byte beyond the limit can be observed
        // and then discarded. `truncated` is therefore decided from the read itself, never
        // from file metadata: a stat could describe a file that grew, shrank, or was
        // replaced by a different inode between validate and open.
        //
        // `truncated` is a WITNESS, not a size comparison:
        //   true  => at least one byte was actually read and discarded.
        //   false => EOF was reached with <= max_bytes bytes available at this read.
        let probe = max_bytes.saturating_add(1); // bounded by MAX_FILE_SIZE above; cannot overflow
        let mut limited = file.take(probe);

        // Read raw bytes to handle binary detection and UTF-8 boundaries correctly.
        // `read_to_end` reports `usize`; convert the u64 cap once with the
        // `try_from(...).unwrap_or(usize::MAX)` idiom used below (never `as usize`,
        // which is `cast_possible_truncation`). Saturating to `usize::MAX` is
        // unreachable here: `max_bytes` is already clamped to MAX_FILE_SIZE above.
        let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);

        let mut raw_bytes = Vec::with_capacity(std::cmp::min(cap, 64 * 1024));
        let probed = limited
            .read_to_end(&mut raw_bytes)
            .map_err(CapabilityError::Io)?;

        let truncated = probed > cap;

        // Discard the probe byte. `bytes_read` counts bytes actually returned, so it is
        // derived AFTER truncation — never from `probed`.
        raw_bytes.truncate(cap);
        let bytes_read = raw_bytes.len() as u64;

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

        let mut out = Output::ok(format!(
            "Read {} bytes from {}{}",
            bytes_read,
            path.display(),
            if truncated { " (truncated)" } else { "" }
        ));
        out.data = Some(data);
        Ok(out)
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
/// Opens a file without O_NOFOLLOW on non-Unix platforms.
///
/// Note: does not use O_NOFOLLOW on non-Unix; symlink swaps between validate and open can redirect reads.
fn open_file_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Detect binary content by checking for null bytes and high ratio of
/// non-printable control characters. Uses a threshold of >10% control chars
/// (excluding common whitespace: \n, \r, \t) to classify as binary.
fn detect_binary(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    // Fast path: null byte = definitely binary
    if data.contains(&0) {
        return true;
    }
    // Count control characters (excluding common whitespace)
    let control_count = data
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    // If more than 10% are control chars, treat as binary
    // Use division to avoid potential multiplication overflow
    control_count > data.len() / 10
}

/// Convert raw bytes to a UTF-8 String, trimming trailing bytes that would
/// split a multibyte character boundary.
fn bytes_to_utf8_string(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            match bytes.get(..valid_up_to) {
                Some(valid_bytes) => match std::str::from_utf8(valid_bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => String::new(),
                },
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_ctx() -> Context {
        Context {
            dry_run: false,
            job_id: "test".into(),
            working_dir: std::env::temp_dir(),
        }
    }

    #[allow(clippy::unwrap_used, clippy::unused_result_ok)]
    #[test]
    fn reads_existing_file() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_read.txt");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            writeln!(f, "hello world").unwrap();
        }

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        let content = result
            .data
            .as_ref()
            .and_then(|d| d.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert!(content.contains("hello world"));
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn rejects_missing_file() {
        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: "/tmp/nonexistent_runtimo_test.txt".to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does not exist") || err.contains("not found"),
            "Expected error about missing file, got: {}",
            err
        );
    }

    #[test]
    fn rejects_empty_path() {
        assert!(TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: String::new(),
                max_bytes: None
            },
            &test_ctx()
        )
        .is_err());
    }

    #[allow(clippy::indexing_slicing)]
    #[allow(clippy::unused_result_ok)]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_max_bytes_limits_output() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_max_bytes.txt");
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            for _ in 0..100 {
                writeln!(f, "hello world line").unwrap();
            }
        }

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(50),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64())
                .unwrap_or(9999)
                <= 50
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_max_bytes_rejects_exceeding_limit() {
        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: "/etc/hosts".to_string(),
                max_bytes: Some(9999999999u64),
            },
            &test_ctx(),
        );
        assert!(result.is_err());
    }

    #[allow(clippy::indexing_slicing)]
    #[test]
    fn test_file_read_default_max_bytes() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_default_max.txt");
        std::fs::write(&tmp, "small content").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn test_file_read_json_parsed_for_agents() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_agent.json");
        std::fs::write(&tmp, r#"{"key": "value", "nested": {"a": 1}}"#).unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        let data = result.data.as_ref().unwrap();
        assert!(data.get("content").unwrap().is_object());
        assert_eq!(
            data.get("content")
                .unwrap()
                .get("key")
                .and_then(|v| v.as_str()),
            Some("value")
        );
        assert_eq!(
            data.get("content")
                .unwrap()
                .get("nested")
                .unwrap()
                .get("a")
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            data.get("content_type").and_then(|v| v.as_str()),
            Some("json")
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_binary_file_detected() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_binary.bin");
        std::fs::write(&tmp, b"hello\x00world").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        let data = result.data.as_ref().unwrap();
        assert_eq!(
            data.get("content_type").and_then(|v| v.as_str()),
            Some("binary")
        );
        assert_eq!(data.get("bytes_read").and_then(|v| v.as_u64()), Some(11));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_utf8_boundary_truncation() {
        // "café" = [99, 97, 102, 195, 169] — é is 2 bytes
        // Truncate at 4 bytes would split the é character
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_utf8.txt");
        std::fs::write(&tmp, b"caf\xc3\xa9").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(4),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        let content = result
            .data
            .as_ref()
            .and_then(|d| d.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(content, "caf");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_bytes_read_reports_raw_bytes() {
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_bytes_read.txt");
        // UTF-8: "café\n" = 6 bytes (é is 2 bytes)
        std::fs::write(&tmp, "café\n").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: None,
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        // bytes_read should be 6 (raw file bytes), not String::len() which is 5
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(6)
        );
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
                let result = TypedCapability::execute(
                    &FileRead,
                    FileReadArgs {
                        path: link_path.to_str().unwrap().to_string(),
                        max_bytes: None,
                    },
                    &test_ctx(),
                );
                assert!(result.is_err(), "symlink should be rejected by O_NOFOLLOW");
                std::fs::remove_file(&link_path).ok();
            }
        }
    }

    // --- F-036: `truncated` must mean "bytes were actually dropped" ----------
    // Before the fix, `limited` was `file.take(max_bytes)`, so `bytes_read ==
    // min(file_size, max_bytes)`. `bytes_read >= max_bytes` therefore evaluated TRUE
    // when file_size == max_bytes, although zero bytes were dropped. `execute` now reads
    // one byte past the cap and discards it, so `truncated` reflects what the read
    // actually observed. The tests below pin both directions of that boundary.

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_truncated_true_when_cap_omits_bytes() {
        // 10-byte file, 9-byte cap: one byte IS dropped, so truncated must be true.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_cap_omits.txt");
        std::fs::write(&tmp, b"0123456789").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(9),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(9)
        );
        assert!(
            result.output.contains("(truncated)"),
            "cap omitted a byte, message must say '(truncated)', got: {}",
            result.output
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_truncated_false_when_max_bytes_equals_file_size() {
        // F-036: file_size == max_bytes. `take()` dropped NOTHING, yet
        // `bytes_read >= max_bytes` reports truncated. This is the defect.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_equal_cap.txt");
        std::fs::write(&tmp, b"0123456789").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(10),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(false),
            "F-036: file_size == max_bytes must NOT report truncated"
        );
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(10),
            "F-036: all 10 bytes were returned"
        );
        assert!(
            !result.output.contains("(truncated)"),
            "F-036: complete read must not claim '(truncated)', got: {}",
            result.output
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_truncated_false_when_max_bytes_exceeds_file_size() {
        // 10-byte file, 11-byte cap: complete read, no truncation.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_cap_exceeds.txt");
        std::fs::write(&tmp, b"0123456789").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(11),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(10)
        );
        assert!(
            !result.output.contains("(truncated)"),
            "cap exceeded file size, message must not say '(truncated)', got: {}",
            result.output
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_truncated_false_when_file_below_cap() {
        // 9-byte file, 10-byte cap: the ordinary below-cap case.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_below_cap.txt");
        std::fs::write(&tmp, b"012345678").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(10),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(9)
        );
        assert!(
            !result.output.contains("(truncated)"),
            "file below cap, message must not say '(truncated)', got: {}",
            result.output
        );
        std::fs::remove_file(&tmp).ok();
    }

    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_truncated_true_when_file_one_byte_over_cap() {
        // 11-byte file, 10-byte cap: exactly one byte IS dropped.
        // This is the counterpart to F-036 — a fix that over-corrects `>=` to `>`
        // (or to a `>= max_bytes + 1` style guard) breaks HERE, not only in T1.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_one_over.txt");
        std::fs::write(&tmp, b"0123456789X").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(10),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("truncated"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "F-036 counterpart: file_size (11) > max_bytes (10) must still report truncated"
        );
        assert_eq!(
            result
                .data
                .as_ref()
                .and_then(|d| d.get("bytes_read"))
                .and_then(|v| v.as_u64()),
            Some(10),
            "take(max_bytes) must cap bytes_read at 10"
        );
        assert!(
            result.output.contains("(truncated)"),
            "one byte dropped, message must say '(truncated)', got: {}",
            result.output
        );
        std::fs::remove_file(&tmp).ok();
    }

    // F-036: the reader caps at `max_bytes + 1` so the one byte past the cap can be
    // OBSERVED (`probed > cap` => truncated) and then DISCARDED by `raw_bytes.truncate(cap)`.
    // The NUL below is deliberately that discarded probe byte: it sits at index 10, exactly
    // `max_bytes` past the start, so `take(11)` reads it but `truncate(10)` must drop it
    // BEFORE `detect_binary` ever sees it.
    //
    // If the truncate is ever moved after `detect_binary`, this 10-byte text file is classified
    // "binary" purely on a byte that was never returned — and the other five F-036 tests still pass.
    // That silent regression is what this test exists to make loud.
    #[allow(clippy::unwrap_used)]
    #[test]
    fn test_probe_byte_never_reaches_output() {
        // 11 bytes: ten ASCII digits, then a NUL as the one-over-cap probe byte.
        let mut tmp = std::env::temp_dir();
        tmp.push("runtimo_test_f036_probe_byte.txt");
        std::fs::write(&tmp, b"0123456789\x00").unwrap();

        let result = TypedCapability::execute(
            &FileRead,
            FileReadArgs {
                path: tmp.to_str().unwrap().to_string(),
                max_bytes: Some(10),
            },
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(result.status, "ok");
        let data = result.data.as_ref().unwrap();

        assert_eq!(
            data.get("bytes_read").and_then(|v| v.as_u64()),
            Some(10),
            "F-036: the probe byte must be discarded, so only 10 bytes are returned"
        );
        assert_eq!(
            data.get("truncated").and_then(|v| v.as_bool()),
            Some(true),
            "F-036: one byte was genuinely omitted, so this is a real truncation"
        );
        assert_eq!(
            data.get("content_type").and_then(|v| v.as_str()),
            Some("text"),
            "F-036: the probe NUL must not reach detect_binary — a leak classifies this \
             text file as \"binary\" on a byte it never returned"
        );
        assert_eq!(
            data.get("content").and_then(|v| v.as_str()),
            Some("0123456789"),
            "F-036: content is the 10 truncated bytes — no NUL, no replacement char"
        );

        std::fs::remove_file(&tmp).ok();
    }
}
