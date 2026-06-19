//! Path validation with canonicalization and prefix checking.
//!
//! Central validation for all path-based capabilities. Handles both existing
//! paths (canonicalize directly) and new paths (canonicalize the parent).
//! Rejects path traversal, empty paths, null bytes, control characters,
//! and paths outside `allowed_prefixes`. Valid UTF-8 paths with non-ASCII
//! characters (e.g. `über.txt`, `中文`) are allowed.
//!
//! Error messages do not leak the list of allowed directories (prevents
//! information disclosure about filesystem layout).
//!
//! # Security Considerations
//!
//! ## Null Byte Rejection (FINDING #8)
//! Paths containing `\0` (null byte) are rejected immediately. Null bytes
//! can truncate C-string path arguments in syscalls, causing path truncation
//! attacks (e.g., `/tmp/safe.txt\0/etc/shadow` becomes `/tmp/safe.txt`).
//!
//! ## Unicode Normalization (FINDING #7)
//! Paths are NFC-normalized before validation to prevent Unicode-based
//! traversal attacks. Non-ASCII paths are allowed after NFC normalization
//! — valid UTF-8 paths with non-ASCII characters (e.g. `über.txt`, `中文`)
//! are accepted. Only control characters (0x00-0x1F, 0x7F) and null bytes
//! are blocked.
//!
//! ## Symlink TOCTOU Limitation (FINDING #9)
//! This module canonicalizes paths via `std::fs::canonicalize()` which
//! resolves symlinks. A TOCTOU window exists between validation and use:
//! an attacker could replace a validated path with a symlink between the
//! two operations. **Mitigation status**: All file-opening capabilities
//! (`FileRead`, `FileWrite`) use `O_NOFOLLOW` flag to prevent symlink
//! attacks at open time. Remaining risk: non-file capabilities (e.g.,
//! `GitExec`, `ShellExec`) may not use `O_NOFOLLOW`. Full mitigation
//! requires filesystem-level atomicity (not available in std).
//!
//! # Configuration
//!
//! Allowed prefixes are merged from three sources (lowest to highest priority):
//! 1. Built-in defaults (`/tmp`, `/var/tmp`, `/home`)
//! 2. `RUNTIMO_ALLOWED_PATHS` env var (colon-separated)
//! 3. Config file `~/.config/runtimo/config.toml` (`allowed_paths` array)
//!
//! Example config file:
//! ```toml
//! allowed_paths = ["/srv", "/opt"]
//! ```

use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Context for path validation.
///
/// Controls which checks are applied. [`Default`] performs all checks
/// with built-in prefixes (`/tmp`, `/var/tmp`, `/home`), extended by
/// `RUNTIMO_ALLOWED_PATHS` env var and config file if set.
#[allow(clippy::exhaustive_structs)]
pub struct PathContext {
    /// Additional allowed directory prefixes (merged with defaults + env var + config).
    pub allowed_prefixes: &'static [&'static str],
    /// If true, the path must already exist on disk.
    pub require_exists: bool,
    /// If true, the path must be a regular file (not a directory).
    pub require_file: bool,
}

impl Default for PathContext {
    fn default() -> Self {
        Self {
            allowed_prefixes: &[],
            require_exists: true,
            require_file: true,
        }
    }
}

/// Returns the full set of allowed path prefixes.
///
/// Combines built-in defaults, `RUNTIMO_ALLOWED_PATHS` env var,
/// config file prefixes, and any context-specific overrides.
fn get_allowed_prefixes(ctx: &PathContext) -> Vec<String> {
    let mut prefixes = crate::config::RuntimoConfig::get_allowed_prefixes();

    // Add context-specific prefixes
    for p in ctx.allowed_prefixes {
        let trimmed = p.trim().to_string();
        if !prefixes.contains(&trimmed) {
            prefixes.push(trimmed);
        }
    }

    prefixes
}

/// Validates a path with canonicalization and prefix checking.
///
/// For existing paths, resolves symlinks via `canonicalize()` to prevent
/// symlink-based escapes. For non-existent paths (writes), canonicalizes
/// the parent directory and appends the filename.
///
/// # CWD Independence (R-C26-01)
///
/// This function is CWD-independent: no `std::env::current_dir()` fallback.
/// Two calls with the same path but different CWD produce identical results
/// or identical errors. Relative paths that do not exist and whose parent
/// does not exist are rejected because they cannot be resolved without CWD.
///
/// # Arguments
/// * `path_str` - Path string to validate
/// * `ctx` - Validation context with allowed prefixes and requirements
///
/// # Returns
/// * `Ok(PathBuf)` - Resolved path (canonical if possible)
/// * `Err(String)` - Validation error message (does not leak allowed prefixes)
///
/// # Errors
/// Returns an error string if the path is empty, contains null bytes,
/// contains control characters, traverses parent directories,
/// does not exist (when required), is not a regular file (when required),
/// cannot be resolved without CWD, or is outside allowed directories.
pub fn validate_path(path_str: &str, ctx: &PathContext) -> Result<PathBuf, String> {
    // Reject empty paths
    if path_str.is_empty() {
        return Err("path is empty".to_string());
    }

    // Reject null bytes — prevents C-string truncation attacks (FINDING #8)
    if path_str.contains('\0') {
        return Err("path contains null byte".to_string());
    }

    // Reject control characters (ASCII 0-31, 127) — can cause terminal
    // injection, log injection, or shell metacharacter issues
    if path_str.chars().any(|c| c.is_control()) {
        return Err("path contains control character".to_string());
    }

    // NFC-normalize the path to prevent Unicode-based traversal (FINDING #7)
    let normalized: String = path_str.nfc().collect();

    // Reject path traversal sequences before any filesystem interaction
    if normalized.contains("..") {
        return Err("path traversal not allowed".to_string());
    }

    let path = Path::new(&normalized);

    // Check existence if required
    if ctx.require_exists && !path.exists() {
        return Err(format!("path does not exist: {}", normalized));
    }

    // Resolve the canonical path:
    // - For existing paths: canonicalize directly (resolves symlinks)
    // - For non-existent paths: canonicalize parent + append filename
    let resolved = if path.exists() {
        path.canonicalize()
            .map_err(|e| format!("canonicalize failed: {}", e))?
    } else {
        // For new files: canonicalize the parent to catch symlink tricks,
        // then join the filename. If parent doesn't exist either, use
        // the path as-is (parent directories will be created at execution time).
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        if parent.exists() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|e| format!("canonicalize parent failed: {}", e))?;
            let filename = path
                .file_name()
                .ok_or_else(|| "invalid filename".to_string())?;
            canonical_parent.join(filename)
        } else {
            // Parent doesn't exist yet — for absolute paths, use as-is.
            // Relative paths rejected to enforce CWD-independent resolution
            // (R-C26-01: same path + different CWD → identical result or error).
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                return Err(format!(
                    "cannot resolve relative path without CWD: {}",
                    normalized
                ));
            }
        }
    };

    // Verify it's a file if required (only meaningful for existing paths)
    if ctx.require_file && resolved.exists() && !resolved.is_file() {
        return Err(format!("not a file: {}", resolved.display()));
    }

    // Check allowed prefixes against the resolved path
    let resolved_str = resolved.to_string_lossy();
    let allowed = get_allowed_prefixes(ctx);
    if !allowed
        .iter()
        .any(|prefix| path_in_prefix(&resolved_str, prefix))
    {
        return Err(format!(
            "path outside allowed directories: {}",
            resolved.display()
        ));
    }

    Ok(resolved)
}

/// Checks if `path` is within `prefix` directory.
///
/// Requires either an exact match or the path starts with `prefix/`.
/// Prevents bypass attacks like `/tmpfoo` matching `/tmp`.
fn path_in_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{}/", prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize tests that set `RUNTIMO_ALLOWED_PATHS` env var.
    static PATH_ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn rejects_empty_path() {
        let ctx = PathContext::default();
        assert!(validate_path("", &ctx).is_err());
    }

    #[test]
    fn rejects_traversal() {
        let ctx = PathContext::default();
        assert!(validate_path("/tmp/../etc/passwd", &ctx).is_err());
    }

    #[test]
    fn accepts_existing_tmp_file() {
        let p = std::env::temp_dir().join("runtimo_val_test.txt");
        std::fs::write(&p, "test").ok();
        let ctx = PathContext::default();
        let result = validate_path(p.to_str().unwrap(), &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn accepts_nonexistent_tmp_file_for_writes() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let result = validate_path("/tmp/runtimo_new_file_test.txt", &ctx);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
    }

    #[test]
    fn rejects_write_outside_allowed() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let result = validate_path("/etc/shadow", &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("outside allowed"));
    }

    #[test]
    fn rejects_symlink_escape() {
        // Create a symlink from /tmp/link -> /etc/hostname
        let link_path = std::env::temp_dir().join("runtimo_symlink_test");
        let _ = std::fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            if symlink("/etc/hostname", &link_path).is_ok() {
                let ctx = PathContext::default();
                let result = validate_path(link_path.to_str().unwrap(), &ctx);
                // Canonicalize resolves the symlink to /etc/hostname → rejected
                assert!(result.is_err(), "symlink escape should be rejected");
                std::fs::remove_file(&link_path).ok();
            }
        }
    }

    #[test]
    fn env_var_extends_allowed_prefixes() {
        let _guard = PATH_ENV_MUTEX.lock().unwrap();
        // /srv is not in defaults, should be rejected
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        assert!(validate_path("/srv/myapp/config", &ctx).is_err());

        // Set env var to allow /srv
        std::env::set_var("RUNTIMO_ALLOWED_PATHS", "/srv:/opt");
        assert!(validate_path("/srv/myapp/config", &ctx).is_ok());
        assert!(validate_path("/opt/tools/bin", &ctx).is_ok());

        // Cleanup
        std::env::remove_var("RUNTIMO_ALLOWED_PATHS");
        assert!(validate_path("/srv/myapp/config", &ctx).is_err());
    }

    #[test]
    fn error_message_does_not_leak_allowed_prefixes() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let err = validate_path("/etc/shadow", &ctx).unwrap_err();
        // Error should not leak the list of allowed directories (info leak)
        assert!(
            !err.contains("/tmp"),
            "error should not leak /tmp as allowed"
        );
        assert!(
            !err.contains("/home"),
            "error should not leak /home as allowed"
        );
        assert!(err.contains("outside allowed directories"));
    }

    #[test]
    fn rejects_null_byte() {
        let ctx = PathContext::default();
        let result = validate_path("/tmp/safe.txt\0/etc/shadow", &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("null byte"));
    }

    #[test]
    fn accepts_non_ascii_path() {
        // Create a file with non-ASCII name on disk first
        let p = std::env::temp_dir().join("café.txt");
        std::fs::write(&p, "test").ok();
        let ctx = PathContext::default();
        let result = validate_path(p.to_str().unwrap(), &ctx);
        // The file exists in a temp dir (allowed prefix), so it should pass
        assert!(
            result.is_ok(),
            "non-ASCII path should be allowed, got: {:?}",
            result
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_non_ascii_unicode_traversal() {
        let ctx = PathContext::default();
        // Unicode homoglyph attack attempt that doesn't exist — should error on "does not exist"
        // The non-ASCII part is now allowed, but the traversal (..) should still be caught
        let result = validate_path("/tmp/\u{00e9}../etc/passwd", &ctx);
        assert!(result.is_err());
        // Should fail due to traversal, not non-ASCII
        assert!(
            !result.unwrap_err().contains("non-ASCII"),
            "should not reject for non-ASCII"
        );
    }

    #[test]
    fn nfc_normalizes_path() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        // NFC normalization should not change ASCII paths
        let result = validate_path("/tmp/normal.txt", &ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_prefix_bypass() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let result = validate_path("/tmpfoo/bar.txt", &ctx);
        assert!(result.is_err(), "/tmpfoo should not match /tmp prefix");
        assert!(result.unwrap_err().contains("outside allowed"));
    }

    #[test]
    fn accepts_valid_prefix_subdir() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let result = validate_path("/tmp/subdir/file.txt", &ctx);
        assert!(result.is_ok(), "/tmp/subdir should match /tmp prefix");
    }

    #[test]
    fn test_path_in_prefix() {
        assert!(path_in_prefix("/tmp", "/tmp"));
        assert!(path_in_prefix("/tmp/foo", "/tmp"));
        assert!(path_in_prefix("/tmp/foo/bar", "/tmp"));
        assert!(!path_in_prefix("/tmpfoo", "/tmp"));
        assert!(!path_in_prefix("/tmpfoo/bar", "/tmp"));
        assert!(!path_in_prefix("/etc/shadow", "/tmp"));
        assert!(path_in_prefix("/home/user/file", "/home"));
        assert!(!path_in_prefix("/homeless/file", "/home"));
    }
}
