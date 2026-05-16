//! Path validation with canonicalization and prefix checking.

use std::path::{Path, PathBuf};

/// Context for path validation.
pub struct PathContext {
    pub allowed_prefixes: &'static [&'static str],
    pub require_exists: bool,
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
/// # Arguments
/// * `path_str` - Path string to validate
/// * `ctx` - Validation context with allowed prefixes and requirements
/// 
/// # Returns
/// * `Ok(PathBuf)` - Canonicalized path
/// * `Err(String)` - Validation error message
pub fn validate_path(path_str: &str, ctx: &PathContext) -> Result<PathBuf, String> {
    // Reject empty paths
    if path_str.is_empty() {
        return Err("path is empty".to_string());
    }

    // Reject path traversal
    if path_str.contains("..") {
        return Err("path traversal not allowed".to_string());
    }

    let path = Path::new(path_str);

    // Check existence if required
    if ctx.require_exists && !path.exists() {
        return Err(format!("path does not exist: {}", path_str));
    }

    // Canonicalize to resolve symlinks
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("canonicalize failed: {}", e))?;

    // Verify it's a file if required
    if ctx.require_file && !canonical.is_file() {
        return Err(format!("not a file: {}", canonical.display()));
    }

    // Check allowed prefixes
    let path_str = canonical.to_string_lossy();
    if !ctx.allowed_prefixes.iter().any(|prefix| path_str.starts_with(prefix)) {
        return Err(format!(
            "path outside allowed directories: {} (resolved from {})",
            canonical.display(),
            path_str
        ));
    }

    Ok(canonical)
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
}
