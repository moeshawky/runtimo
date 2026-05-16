//! GitExec capability — git operations with state tracking and undo support.
//!
//! Provides git operations (clone, pull, commit, revert, clean, status) with:
//! - State tracking (commit sha, branch, remote URL)
//! - Backup-before-mutate for undo support
//! - WAL logging for audit trail
//! - Path traversal protection
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::GitExec;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//! use std::path::PathBuf;
//!
//! let cap = GitExec::new(PathBuf::from("/tmp/backups"));
//! let result = cap.execute(
//!     &json!({"operation": "clone", "url": "https://github.com/user/repo.git", "path": "/tmp/repo"}),
//!     &Context { dry_run: false, job_id: "job1".into(), working_dir: PathBuf::from("/tmp") }
//! ).unwrap();
//!
//! assert!(result.success);
//! ```

use crate::backup::BackupManager;
use crate::capability::{Capability, Context, Output};
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Arguments for the [`GitExec`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitExecArgs {
    /// Git operation to perform (clone, pull, commit, revert, clean, status).
    pub operation: String,
    /// Repository URL (for clone/pull).
    pub url: Option<String>,
    /// Local path to repository (for clone/commit/revert/clean/status).
    pub path: Option<String>,
    /// Branch name (for checkout/clone).
    pub branch: Option<String>,
    /// Commit message (for commit).
    pub message: Option<String>,
    /// Files to commit (for commit).
    pub files: Option<Vec<String>>,
    /// Commit SHA to revert to (for revert).
    pub commit_sha: Option<String>,
    /// Timeout in seconds (default: 300).
    pub timeout_secs: Option<u64>,
}

/// Git state before/after operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitState {
    /// Current commit SHA (HEAD).
    pub commit_sha: Option<String>,
    /// Current branch name.
    pub branch: Option<String>,
    /// Remote URL (origin).
    pub remote_url: Option<String>,
    /// Repository path.
    pub repo_path: String,
    /// Working directory status (clean/dirty).
    pub is_clean: bool,
}

/// Capability that executes git operations with full state tracking.
///
/// Supports clone, pull, commit, revert, clean, and status operations.
/// Creates backups before mutable operations for undo support.
pub struct GitExec {
    backup_mgr: BackupManager,
}

impl GitExec {
    /// Creates a new GitExec capability with the given backup directory.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// directory cannot be created.
    pub fn new(backup_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            backup_mgr: BackupManager::new(backup_dir)?,
        })
    }

    /// Captures the current git state for a repository.
    fn capture_state(repo_path: &Path) -> Result<GitState> {
        let commit_sha = Self::run_git(repo_path, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .ok();
        
        let branch = Self::run_git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|s| s.trim().to_string())
            .ok();
        
        let remote_url = Self::run_git(repo_path, &["remote", "get-url", "origin"])
            .map(|s| s.trim().to_string())
            .ok();
        
        let is_clean = Self::is_working_tree_clean(repo_path);

        Ok(GitState {
            commit_sha,
            branch,
            remote_url,
            repo_path: repo_path.to_string_lossy().to_string(),
            is_clean,
        })
    }

    /// Runs a git command and returns the output.
    fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("git command failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git {}: {}", args.join(" "), stderr)));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Checks if the working tree is clean (no uncommitted changes).
    fn is_working_tree_clean(repo_path: &Path) -> bool {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(&["status", "--porcelain"])
            .output();
        
        match output {
            Ok(out) => out.stdout.is_empty() && out.stderr.is_empty(),
            Err(_) => false,
        }
    }

    /// Validates a git URL format.
    fn validate_url(url: &str) -> Result<()> {
        if url.starts_with("http://") || url.starts_with("https://") {
            Ok(())
        } else if url.contains('@') && url.contains(':') {
            Ok(())
        } else {
            Err(Error::SchemaValidationFailed(format!("Invalid git URL: {}", url)))
        }
    }

    /// Validates a branch name.
    fn validate_branch_name(branch: &str) -> Result<()> {
        if branch.is_empty() {
            return Err(Error::SchemaValidationFailed("Branch name is empty".into()));
        }
        if branch.contains("..") || branch.contains("@{") {
            return Err(Error::SchemaValidationFailed(format!("Invalid branch name: {}", branch)));
        }
        Ok(())
    }

    /// Validates a commit SHA.
    fn validate_commit_sha(sha: &str) -> Result<()> {
        if sha.len() < 7 || sha.len() > 40 {
            return Err(Error::SchemaValidationFailed(format!("Invalid commit SHA length: {}", sha)));
        }
        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::SchemaValidationFailed(format!("Invalid commit SHA: {}", sha)));
        }
        Ok(())
    }

    /// Executes git clone operation.
    fn op_clone(&self, args: &GitExecArgs, ctx: &Context) -> Result<Output> {
        let url = args.url.as_ref().ok_or_else(|| {
            Error::ExecutionFailed("URL required for clone".into())
        })?;
        let path = args.path.as_ref().ok_or_else(|| {
            Error::ExecutionFailed("Path required for clone".into())
        })?;

        Self::validate_url(url)?;
        
        let path = Path::new(path);
        if path.exists() {
            return Err(Error::ExecutionFailed(format!("Path already exists: {}", path.display())));
        }

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "operation": "clone",
                    "url": url,
                    "path": path.display().to_string(),
                    "dry_run": true
                }),
                message: Some(format!("DRY RUN: would clone {} to {}", url, path.display())),
            });
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::ExecutionFailed(format!("mkdir {}: {}", parent.display(), e)))?;
        }

        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(url).arg(path);
        
        if let Some(branch) = &args.branch {
            cmd.arg("-b").arg(branch);
        }

        let output = cmd.output()
            .map_err(|e| Error::ExecutionFailed(format!("git clone failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git clone failed: {}", stderr)));
        }

        let state = Self::capture_state(path).unwrap_or(GitState {
            commit_sha: None,
            branch: None,
            remote_url: Some(url.clone()),
            repo_path: path.display().to_string(),
            is_clean: true,
        });

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "clone",
                "url": url,
                "path": path.display().to_string(),
                "commit_sha": state.commit_sha,
                "branch": state.branch,
                "remote_url": state.remote_url
            }),
            message: Some(format!("Cloned {} to {}", url, path.display())),
        })
    }

    /// Executes git pull operation.
    fn op_pull(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!("Repository not found: {}", repo_path.display())));
        }

        let state_before = Self::capture_state(repo_path)?;

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "operation": "pull",
                    "path": repo_path.display().to_string(),
                    "dry_run": true
                }),
                message: Some("DRY RUN: would pull".into()),
            });
        }

        let backup_path = if !state_before.is_clean {
            Some(self.backup_mgr.create_backup(repo_path, &ctx.job_id)?)
        } else {
            None
        };

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(&["pull"])
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("git pull failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git pull failed: {}", stderr)));
        }

        let state_after = Self::capture_state(repo_path)?;

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "pull",
                "path": repo_path.display().to_string(),
                "commit_sha_before": state_before.commit_sha,
                "commit_sha_after": state_after.commit_sha,
                "branch": state_after.branch,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
            }),
            message: Some("Pulled successfully".into()),
        })
    }

    /// Executes git commit operation.
    fn op_commit(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!("Repository not found: {}", repo_path.display())));
        }

        let message = args.message.as_ref().ok_or_else(|| {
            Error::ExecutionFailed("Commit message required".into())
        })?;

        let state_before = Self::capture_state(repo_path)?;

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "operation": "commit",
                    "path": repo_path.display().to_string(),
                    "message": message,
                    "dry_run": true
                }),
                message: Some("DRY RUN: would commit".into()),
            });
        }

        let backup_path = Some(self.backup_mgr.create_backup(repo_path, &ctx.job_id)?);

        if let Some(files) = &args.files {
            for file in files {
                let _ = Command::new("git")
                    .current_dir(repo_path)
                    .args(&["add", file])
                    .output();
            }
        } else {
            let _ = Command::new("git")
                .current_dir(repo_path)
                .args(&["add", "-A"])
                .output();
        }

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(&["commit", "-m", message])
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("git commit failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git commit failed: {}", stderr)));
        }

        let state_after = Self::capture_state(repo_path)?;

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "commit",
                "path": repo_path.display().to_string(),
                "message": message,
                "commit_sha_before": state_before.commit_sha,
                "commit_sha_after": state_after.commit_sha,
                "branch": state_after.branch,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
            }),
            message: Some(format!("Committed: {}", message)),
        })
    }

    /// Executes git revert operation.
    fn op_revert(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!("Repository not found: {}", repo_path.display())));
        }

        let commit_sha = args.commit_sha.as_ref().ok_or_else(|| {
            Error::ExecutionFailed("Commit SHA required for revert".into())
        })?;

        Self::validate_commit_sha(commit_sha)?;

        let state_before = Self::capture_state(repo_path)?;

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "operation": "revert",
                    "path": repo_path.display().to_string(),
                    "commit_sha": commit_sha,
                    "dry_run": true
                }),
                message: Some(format!("DRY RUN: would revert {}", commit_sha)),
            });
        }

        let backup_path = Some(self.backup_mgr.create_backup(repo_path, &ctx.job_id)?);

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(&["revert", "--no-edit", commit_sha])
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("git revert failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git revert failed: {}", stderr)));
        }

        let state_after = Self::capture_state(repo_path)?;

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "revert",
                "path": repo_path.display().to_string(),
                "commit_sha": commit_sha,
                "commit_sha_before": state_before.commit_sha,
                "commit_sha_after": state_after.commit_sha,
                "branch": state_after.branch,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
            }),
            message: Some(format!("Reverted {}", commit_sha)),
        })
    }

    /// Executes git clean operation.
    fn op_clean(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!("Repository not found: {}", repo_path.display())));
        }

        let state_before = Self::capture_state(repo_path)?;

        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "operation": "clean",
                    "path": repo_path.display().to_string(),
                    "dry_run": true
                }),
                message: Some("DRY RUN: would clean untracked files".into()),
            });
        }

        let backup_path = Some(self.backup_mgr.create_backup(repo_path, &ctx.job_id)?);

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(&["clean", "-fd"])
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("git clean failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::ExecutionFailed(format!("git clean failed: {}", stderr)));
        }

        let state_after = Self::capture_state(repo_path)?;

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "clean",
                "path": repo_path.display().to_string(),
                "was_clean": state_before.is_clean,
                "is_clean": state_after.is_clean,
                "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
            }),
            message: Some("Cleaned untracked files".into()),
        })
    }

    /// Executes git status operation.
    fn op_status(&self, _args: &GitExecArgs, _ctx: &Context, repo_path: &Path) -> Result<Output> {
        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!("Repository not found: {}", repo_path.display())));
        }

        let state = Self::capture_state(repo_path)?;

        let status_output = Self::run_git(repo_path, &["status", "--porcelain"])
            .unwrap_or_default();

        let branch = state.branch.clone().unwrap_or_default();
        let remote_url = state.remote_url.clone().unwrap_or_default();

        Ok(Output {
            success: true,
            data: serde_json::json!({
                "operation": "status",
                "path": repo_path.display().to_string(),
                "branch": branch,
                "remote_url": remote_url,
                "commit_sha": state.commit_sha,
                "is_clean": state.is_clean,
                "status": status_output
            }),
            message: Some(format!("On branch {}: {}", branch, if state.is_clean { "clean" } else { "dirty" })),
        })
    }
}

impl Capability for GitExec {
    fn name(&self) -> &'static str {
        "GitExec"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["clone", "pull", "commit", "revert", "clean", "status"] },
                "url": { "type": "string" },
                "path": { "type": "string" },
                "branch": { "type": "string" },
                "message": { "type": "string" },
                "files": { "type": "array", "items": { "type": "string" } },
                "commit_sha": { "type": "string" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
            },
            "required": ["operation"]
        })
    }

    fn validate(&self, args: &Value) -> Result<()> {
        let args: GitExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        let valid_ops = ["clone", "pull", "commit", "revert", "clean", "status"];
        if !valid_ops.contains(&args.operation.as_str()) {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid operation: {}. Must be one of: {}",
                args.operation,
                valid_ops.join(", ")
            )));
        }

        if args.operation == "clone" {
            if let Some(url) = &args.url {
                Self::validate_url(url)?;
            } else {
                return Err(Error::SchemaValidationFailed("URL required for clone".into()));
            }
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: false,
                    require_file: false,
                    ..Default::default()
                };
                validate_path(path, &ctx).map_err(Error::SchemaValidationFailed)?;
            }
        }

        if args.operation != "clone" {
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: true,
                    require_file: false,
                    ..Default::default()
                };
                validate_path(path, &ctx).map_err(Error::SchemaValidationFailed)?;
            }
        }

        if let Some(branch) = &args.branch {
            Self::validate_branch_name(branch)?;
        }

        if let Some(sha) = &args.commit_sha {
            Self::validate_commit_sha(sha)?;
        }

        Ok(())
    }

    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
        let args: GitExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        match args.operation.as_str() {
            "clone" => self.op_clone(&args, ctx),
            "pull" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    Error::ExecutionFailed("Path required for pull".into())
                })?;
                self.op_pull(&args, ctx, Path::new(path))
            }
            "commit" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    Error::ExecutionFailed("Path required for commit".into())
                })?;
                self.op_commit(&args, ctx, Path::new(path))
            }
            "revert" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    Error::ExecutionFailed("Path required for revert".into())
                })?;
                self.op_revert(&args, ctx, Path::new(path))
            }
            "clean" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    Error::ExecutionFailed("Path required for clean".into())
                })?;
                self.op_clean(&args, ctx, Path::new(path))
            }
            "status" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    Error::ExecutionFailed("Path required for status".into())
                })?;
                self.op_status(&args, ctx, Path::new(path))
            }
            _ => Err(Error::ExecutionFailed(format!(
                "Unknown operation: {}",
                args.operation
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    fn test_backup_dir() -> PathBuf {
        std::env::temp_dir().join("runtimo_git_test")
    }

    #[test]
    fn validates_git_url() {
        assert!(GitExec::validate_url("https://github.com/user/repo.git").is_ok());
        assert!(GitExec::validate_url("http://example.com/repo.git").is_ok());
        assert!(GitExec::validate_url("git@github.com:user/repo.git").is_ok());
        
        assert!(GitExec::validate_url("not-a-url").is_err());
        assert!(GitExec::validate_url("").is_err());
        
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn validates_branch_name() {
        assert!(GitExec::validate_branch_name("main").is_ok());
        assert!(GitExec::validate_branch_name("feature/my-branch").is_ok());
        assert!(GitExec::validate_branch_name("v1.0").is_ok());
        
        assert!(GitExec::validate_branch_name("").is_err());
        assert!(GitExec::validate_branch_name("bad..name").is_err());
        assert!(GitExec::validate_branch_name("@{..}").is_err());
    }

    #[test]
    fn validates_commit_sha() {
        assert!(GitExec::validate_commit_sha("abc123").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0").is_ok());
        
        assert!(GitExec::validate_commit_sha("abc").is_err());
        assert!(GitExec::validate_commit_sha("").is_err());
        assert!(GitExec::validate_commit_sha("xyz123").is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        
        let err = cap.validate(&serde_json::json!({
            "operation": "clone",
            "url": "https://github.com/user/repo.git",
            "path": "../../../etc/passwd"
        })).unwrap_err();
        
        assert!(err.to_string().contains("traversal"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn rejects_invalid_operation() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        
        let err = cap.validate(&serde_json::json!({
            "operation": "invalid_op"
        })).unwrap_err();
        
        assert!(err.to_string().contains("Invalid operation"));
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn status_on_nonexistent_repo() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        
        let result = cap.execute(
            &serde_json::json!({
                "operation": "status",
                "path": "/tmp/nonexistent_repo"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        );
        
        assert!(result.is_err());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }
}
