//! Path validation with canonicalization and prefix checking.
//!
//! Central validation for all path-based capabilities. Handles both existing
//! paths (canonicalize directly) and new paths (canonicalize the parent).
//! Rejects path traversal, empty paths, and paths outside `allowed_prefixes`.
//!
//! # Configuration
//!
//! Allowed prefixes can be extended via the `RUNTIMO_ALLOWED_PATHS` environment
//! variable (colon-separated). Example:
//! ```bash
//! export RUNTIMO_ALLOWED_PATHS="/tmp:/var/tmp:/home:/srv:/opt"
//! ```
//! This is merged with the built-in defaults (`/tmp`, `/var/tmp`, `/home`).

use std::path::{Path, PathBuf};

/// Built-in default allowed prefixes.
const DEFAULT_PREFIXES: &[&str] = &["/tmp", "/var/tmp", "/home"];

/// Context for path validation.
///
/// Controls which checks are applied. [`Default`] enables all checks
/// with built-in prefixes (`/tmp`, `/var/tmp`, `/home`), extended by
/// `RUNTIMO_ALLOWED_PATHS` env var if set.
pub struct PathContext {
    /// Additional allowed directory prefixes (merged with defaults + env var).
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
/// Combines built-in defaults with `RUNTIMO_ALLOWED_PATHS` env var
/// (colon-separated) and any context-specific prefixes.
fn get_allowed_prefixes(ctx: &PathContext) -> Vec<String> {
    let mut prefixes: Vec<String> = DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect();

    // Extend with env var (colon-separated)
    if let Ok(env_paths) = std::env::var("RUNTIMO_ALLOWED_PATHS") {
        for p in env_paths.split(':').filter(|s| !s.is_empty()) {
            let trimmed = p.trim().to_string();
            if !prefixes.contains(&trimmed) {
                prefixes.push(trimmed);
            }
        }
    }

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
}
