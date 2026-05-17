//! Path validation with canonicalization and prefix checking.
//!
//! Central validation for all path-based capabilities. Handles both existing
//! paths (canonicalize directly) and new paths (canonicalize the parent).
//! Rejects path traversal, empty paths, null bytes, non-ASCII paths, and
//! paths outside `allowed_prefixes`.
//!
//! # Security Considerations
//!
//! ## Null Byte Rejection (FINDING #8)
//! Paths containing `\0` (null byte) are rejected immediately. Null bytes
//! can truncate C-string path arguments in syscalls, enabling path truncation
//! attacks (e.g., `/tmp/safe.txt\0/etc/shadow` becomes `/tmp/safe.txt`).
//!
//! ## Unicode Normalization (FINDING #7)
//! Paths are NFC-normalized before validation to prevent Unicode-based
//! traversal attacks. Non-ASCII paths are rejected entirely because Unicode
//! normalization edge cases (e.g., homoglyphs, combining characters) cannot
//! be fully mitigated without filesystem-level awareness.
//!
//! ## Symlink TOCTOU Limitation (FINDING #9)
//! This module canonicalizes paths via `std::fs::canonicalize()` which
//! resolves symlinks. However, a TOCTOU window exists between validation
//! and use: an attacker could replace a validated path with a symlink
//! between the two operations. Mitigations:
//! - Use `O_NOFOLLOW` flag when opening files (where possible)
//! - Minimize time between validation and use
//! - Full mitigation requires filesystem-level atomicity (not available in std)
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
/// Controls which checks are applied. [`Default`] enables all checks
/// with built-in prefixes (`/tmp`, `/var/tmp`, `/home`), extended by
/// `RUNTIMO_ALLOWED_PATHS` env var and config file if set.
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
/// # Arguments
/// * `path_str` - Path string to validate
/// * `ctx` - Validation context with allowed prefixes and requirements
///
/// # Returns
/// * `Ok(PathBuf)` - Resolved path (canonical if possible)
/// * `Err(String)` - Validation error message
pub fn validate_path(path_str: &str, ctx: &PathContext) -> Result<PathBuf, String> {
    // Reject empty paths
    if path_str.is_empty() {
        return Err("path is empty".to_string());
    }

    // Reject null bytes — prevents C-string truncation attacks (FINDING #8)
    if path_str.contains('\0') {
        return Err("path contains null byte".to_string());
    }

    // Reject non-ASCII paths — Unicode normalization edge cases cannot be
    // fully mitigated without filesystem-level awareness (FINDING #7)
    if !path_str.is_ascii() {
        return Err("non-ASCII paths are not supported".to_string());
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
        let parent = path.parent().unwrap_or(Path::new("/"));
        if parent.exists() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|e| format!("canonicalize parent failed: {}", e))?;
            let filename = path
                .file_name()
                .ok_or_else(|| "invalid filename".to_string())?;
            canonical_parent.join(filename)
        } else {
            // Parent doesn't exist yet — convert to absolute for prefix check.
            // This handles the case where `create_dir_all` will create parents.
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .map_err(|e| format!("cannot resolve relative path: {}", e))?
                    .join(path)
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
        .any(|prefix| resolved_str.starts_with(prefix.as_str()))
    {
        return Err(format!(
            "path outside allowed directories: {} (allowed: {})",
            resolved.display(),
            allowed.join(", ")
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn error_message_shows_allowed_prefixes() {
        let ctx = PathContext {
            require_exists: false,
            require_file: false,
            ..Default::default()
        };
        let err = validate_path("/etc/shadow", &ctx).unwrap_err();
        assert!(err.contains("/tmp"), "error should list /tmp as allowed");
        assert!(err.contains("/home"), "error should list /home as allowed");
    }

    #[test]
    fn rejects_null_byte() {
        let ctx = PathContext::default();
        let result = validate_path("/tmp/safe.txt\0/etc/shadow", &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("null byte"));
    }

    #[test]
    fn rejects_non_ascii_path() {
        let ctx = PathContext::default();
        let result = validate_path("/tmp/café.txt", &ctx);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-ASCII"));
    }

    #[test]
    fn rejects_non_ascii_unicode_traversal() {
        let ctx = PathContext::default();
        // Unicode homoglyph attack attempt
        let result = validate_path("/tmp/\u{00e9}../etc/passwd", &ctx);
        assert!(result.is_err());
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
}
