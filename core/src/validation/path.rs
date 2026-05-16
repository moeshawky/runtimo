//! Path validation with canonicalization and prefix checking.
//!
//! Central validation for all path-based capabilities. Handles both existing
//! paths (canonicalize directly) and new paths (canonicalize the parent).
//! Rejects path traversal, empty paths, and paths outside `allowed_prefixes`.

use std::path::{Path, PathBuf};

/// Context for path validation.
///
/// Controls which checks are applied. [`Default`] enables all checks
/// with prefixes `["/tmp", "/var/tmp", "/home"]`.
pub struct PathContext {
    /// Allowed directory prefixes (canonical form).
    pub allowed_prefixes: &'static [&'static str],
    /// If true, the path must already exist on disk.
    pub require_exists: bool,
    /// If true, the path must be a regular file (not a directory).
    pub require_file: bool,
}

impl Default for PathContext {
    fn default() -> Self {
        Self {
            allowed_prefixes: &["/tmp", "/var/tmp", "/home"],
            require_exists: true,
            require_file: true,
        }
    }
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

    // Reject path traversal sequences before any filesystem interaction
    if path_str.contains("..") {
        return Err("path traversal not allowed".to_string());
    }

    let path = Path::new(path_str);

    // Check existence if required
    if ctx.require_exists && !path.exists() {
        return Err(format!("path does not exist: {}", path_str));
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
            let filename = path.file_name()
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
    if !ctx.allowed_prefixes.iter().any(|prefix| resolved_str.starts_with(prefix)) {
        return Err(format!(
            "path outside allowed directories: {}",
            resolved.display(),
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
}
