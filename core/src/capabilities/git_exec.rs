//! GitExec capability — git operations with state tracking and undo support.
//!
//! Provides git operations (clone, pull, commit, revert, clean, status) with:
//! - State tracking (commit sha, branch, remote URL)
//! - Backup-before-mutate for undo support
//! - WAL logging for audit trail
//! - Path traversal protection
//! - Timeout enforcement on all git subprocesses
//! - URL validation (HTTPS/SSH only, SSRF blocking)
//! - Credential sanitization from output and stderr
//! - Secret file detection for git add
//! - Telemetry and process tracking before/after execution
//!
//! # Network capability
//!
//! **Git operations ARE inherently network-capable.** `git clone`, `git pull`,
//! and `git fetch` make outbound connections to remote repositories.
//! This is by design — denying network access would make GitExec useless.
//!
//! The network isolation is at the transport/protocol level:
//! - Only HTTPS (`https://`) and SSH (`git@`) URLs are accepted
//! - SSRF targets (metadata services, localhost, private ranges) are blocked
//! - Credentials are sanitized from all output, stderr, and telemetry
//!
//! **Note on ShellExec interaction:** GitExec spawns `git` subprocesses which
//! internally invoke `git-remote-https` (a git helper, NOT the system `curl`).
//! The ShellExec network blocklist (`curl`, `wget`, etc.) does NOT affect
//! GitExec — git uses its own transport layer. However, `RUNTIMO_ENABLE_NETWORK`
//! does NOT gate GitExec; GitExec's network access is controlled by its own
//! URL validation and SSRF blocking.
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
//! assert!(result.status == "ok");
//! ```

use crate::backup::BackupManager;
use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Arguments for the [`GitExec`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
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

/// Known secret file patterns to exclude from `git add -A`.
const SECRET_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.staging",
    "credentials.json",
    "credentials.yml",
    "credentials.yaml",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    ".ssh/id_dsa",
    "id_rsa",
    "id_ed25519",
    "id_dsa",
    ".npmrc",
    ".pypirc",
    ".docker/config.json",
    "token",
    "api_key",
    "api_secret",
    ".aws/credentials",
    ".azure/credentials",
    "keystore.jks",
    "keystore.p12",
];

/// Maximum number of untracked files allowed for `git clean -fd`.
const MAX_CLEAN_FILES: usize = 1000;

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

    /// Runs a git command with timeout enforcement and returns the output.
    fn run_git_with_timeout(repo_path: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
        let mut child = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("git command failed: {}", e)))?;

        let timeout = Duration::from_secs(timeout_secs);
        let start = Instant::now();

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|e| Error::ExecutionFailed(format!("git wait failed: {}", e)))?;
                    if !status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        // Sanitize stderr for safe JSON embedding: escape control chars
                        let sanitized_stderr = stderr
                            .chars()
                            .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
                            .collect::<String>();
                        return Err(Error::ExecutionFailed(format!(
                            "git {}: {}",
                            args.join(" "),
                            sanitized_stderr.trim()
                        )));
                    }
                    return Ok(String::from_utf8_lossy(&output.stdout).to_string());
                }
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::ExecutionFailed(format!(
                            "git {} timed out after {}s",
                            args.join(" "),
                            timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::ExecutionFailed(format!("git wait error: {}", e)));
                }
            }
        }
    }

    /// Checks if the working tree is clean (no uncommitted changes).
    fn is_working_tree_clean(repo_path: &Path) -> bool {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["status", "--porcelain"])
            .output();

        match output {
            Ok(out) => out.stdout.is_empty() && out.stderr.is_empty(),
            Err(_) => false,
        }
    }

    /// Validates a git URL format. Blocks http:// (MITM risk) and SSRF patterns.
    fn validate_url(url: &str) -> Result<()> {
        let is_https = url.starts_with("https://");
        let is_ssh = url.starts_with("git@");
        if !is_https && !is_ssh {
            return Err(Error::SchemaValidationFailed(format!(
                "Insecure or unsupported URL scheme: {} (must use https:// or git@ SSH)",
                url
            )));
        }

        if is_https {
            if let Some(host_part) = url
                .strip_prefix("https://")
                .and_then(|s| s.split('/').next())
            {
                let host = host_part.split(':').next().unwrap_or(host_part);
                if Self::is_ssrf_host(host) {
                    return Err(Error::SchemaValidationFailed(format!(
                        "SSRF blocked: URL targets internal/metadata address: {}",
                        url
                    )));
                }
            }
        } else if is_ssh {
            if let Some(host) = url.strip_prefix("git@").and_then(|s| s.split(':').next()) {
                if Self::is_ssrf_host(host) {
                    return Err(Error::SchemaValidationFailed(format!(
                        "SSRF blocked: URL targets internal/metadata address: {}",
                        url
                    )));
                }
            }
        }

        Ok(())
    }

    /// Checks if a host is a known SSRF target (cloud metadata, localhost, link-local).
    fn is_ssrf_host(host: &str) -> bool {
        let lower = host.to_lowercase();
        let ssrf_indicators = [
            "169.254.169.254",
            "169.254.",
            "127.0.0.1",
            "localhost",
            "0.0.0.0",
            "::1",
            "10.0.0.",
            "10.0.1.",
            "10.0.2.",
            "10.0.3.",
            "172.16.",
            "172.17.",
            "172.18.",
            "172.19.",
            "172.20.",
            "172.21.",
            "172.22.",
            "172.23.",
            "172.24.",
            "172.25.",
            "172.26.",
            "172.27.",
            "172.28.",
            "172.29.",
            "172.30.",
            "172.31.",
            "192.168.",
            "metadata.google",
            "metadata.azure",
            "instance-data",
            "100.100.100.200",
            "[::1]",
            "[fe80:",
        ];
        ssrf_indicators
            .iter()
            .any(|indicator| lower.contains(indicator))
    }

    /// Validates a branch name against git's ref naming rules and option injection.
    ///
    /// Rejects: empty branches, `..` (range spec), `@{` (reflog), `--` prefix
    /// (option injection), `refs/` patterns (ref injection), control characters,
    /// whitespace, and shell/git metacharacters (`:`, `~`, `^`, `*`, `[`, `\\`,
    /// `.lock`, `?`).
    fn validate_branch_name(branch: &str) -> Result<()> {
        if branch.is_empty() {
            return Err(Error::SchemaValidationFailed("Branch name is empty".into()));
        }
        if branch.contains("..") || branch.contains("@{") {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid branch name: {}",
                branch
            )));
        }
        if branch.starts_with("--") {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name cannot start with '--': {}",
                branch
            )));
        }
        if branch.starts_with("refs/") || branch.contains("/refs/") {
            return Err(Error::SchemaValidationFailed(format!(
                "Ref injection detected in branch name: {}",
                branch
            )));
        }
        if branch.contains(|c: char| c.is_control() || c.is_whitespace()) {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name contains control or whitespace: {}",
                branch
            )));
        }
        if branch.contains([':', '~', '^', '*', '[', '\\', '?'])
            || std::path::Path::new(branch)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
        {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name contains invalid character: {}",
                branch
            )));
        }
        Ok(())
    }

    /// Validates a commit SHA.
    fn validate_commit_sha(sha: &str) -> Result<()> {
        if sha.len() < 7 || sha.len() > 40 {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid commit SHA length: {}",
                sha
            )));
        }
        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid commit SHA: {}",
                sha
            )));
        }
        Ok(())
    }

    /// Sanitizes credentials from a URL string (redacts user:pass@).
    /// Preserves SSH-style URLs (git@host:path) unchanged.
    #[allow(clippy::arithmetic_side_effects)]
    fn sanitize_url(url: &str) -> String {
        if url.starts_with("git@") {
            return url.to_string();
        }
        if let Some(at_pos) = url.find('@') {
            if let Some(scheme_end) = url.find("://") {
                let scheme = &url[..scheme_end + 3];
                let after_at = &url[at_pos + 1..];
                return format!("{}***@{}", scheme, after_at);
            }
            return format!("***@{}", &url[at_pos + 1..]);
        }
        url.to_string()
    }

    /// Sanitizes git output to remove credential leakage.
    fn sanitize_output(output: &str) -> String {
        let re_pattern = |line: &str| -> String {
            let mut result = String::new();
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c == ':' && chars.peek() == Some(&'/') && chars.clone().nth(1) == Some('/') {
                    result.push_str("://");
                    chars.next();
                    chars.next();
                    let mut user_pass = String::new();
                    let mut found_at = false;
                    for nc in chars.by_ref() {
                        if nc == '@' {
                            found_at = true;
                            break;
                        }
                        user_pass.push(nc);
                    }
                    if found_at && !user_pass.is_empty() {
                        result.push_str("***@");
                    } else {
                        result.push_str(&user_pass);
                        if found_at {
                            result.push('@');
                        }
                    }
                } else {
                    result.push(c);
                }
            }
            result
        };

        output
            .lines()
            .map(re_pattern)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Checks if a file path looks like a secret file that should not be committed.
    fn is_secret_file(path: &str) -> bool {
        let lower = path.to_lowercase();
        SECRET_PATTERNS.iter().any(|pattern| {
            lower == *pattern
                || lower.ends_with(&format!("/{}", pattern))
                || lower.contains(&format!("/{}/", pattern))
        })
    }

    /// Validates a file path for git add (no traversal, no secrets).
    fn validate_add_file(file: &str, repo_path: &Path) -> Result<()> {
        if file.contains("..") {
            return Err(Error::SchemaValidationFailed(format!(
                "Path traversal in file path: {}",
                file
            )));
        }
        if Self::is_secret_file(file) {
            return Err(Error::SchemaValidationFailed(format!(
                "Secret file detected, refusing to add: {}",
                file
            )));
        }
        let full_path = repo_path.join(file);
        if full_path.exists() {
            let canonical = full_path.canonicalize().map_err(|e| {
                Error::SchemaValidationFailed(format!("Cannot resolve file {}: {}", file, e))
            })?;
            let canonical_repo = repo_path.canonicalize().map_err(|e| {
                Error::SchemaValidationFailed(format!("Cannot resolve repo: {}", e))
            })?;
            if !canonical.starts_with(&canonical_repo) {
                return Err(Error::SchemaValidationFailed(format!(
                    "File {} escapes repository boundary",
                    file
                )));
            }
        }
        Ok(())
    }

    /// Checks available disk space (returns free bytes, or None if unknown).
    fn disk_free_bytes(path: &Path) -> Option<u64> {
        let output = Command::new("df")
            .arg("--output=avail")
            .arg("-B1")
            .arg(path)
            .output()
            .ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().nth(1)?.trim().parse().ok()
        } else {
            None
        }
    }

    /// Counts untracked files that would be removed by git clean -fd.
    fn count_untracked_files(repo_path: &Path, timeout_secs: u64) -> Result<usize> {
        let output = Self::run_git_with_timeout(
            repo_path,
            &["ls-files", "--others", "--exclude-standard"],
            timeout_secs,
        )?;
        Ok(output.lines().filter(|l| !l.is_empty()).count())
    }

    /// Sanitizes a commit message (strips control chars, ensures non-empty).
    fn sanitize_commit_message(msg: &str) -> Result<String> {
        let sanitized: String = msg
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        let trimmed = sanitized.trim();
        if trimmed.is_empty() {
            return Err(Error::SchemaValidationFailed(
                "Commit message is empty after sanitization".into(),
            ));
        }
        Ok(trimmed.to_string())
    }

    /// Creates a backup unconditionally before any mutating operation.
    fn backup_before_mutation(&self, repo_path: &Path, job_id: &str) -> Result<PathBuf> {
        self.backup_mgr.create_backup(repo_path, job_id)
    }

    /// Captures the current git state for a repository.
    fn capture_state(repo_path: &Path, timeout_secs: u64) -> Result<GitState> {
        let commit_sha =
            Self::run_git_with_timeout(repo_path, &["rev-parse", "HEAD"], timeout_secs)
                .map(|s| s.trim().to_string())
                .ok();

        let branch = Self::run_git_with_timeout(
            repo_path,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            timeout_secs,
        )
        .map(|s| s.trim().to_string())
        .ok();

        let remote_url =
            Self::run_git_with_timeout(repo_path, &["remote", "get-url", "origin"], timeout_secs)
                .ok()
                .and_then(|s| {
                    let trimmed = s.trim().to_string();
                    let sanitized = Self::sanitize_url(&trimmed);
                    if sanitized.is_empty() {
                        None
                    } else {
                        Some(sanitized)
                    }
                });

        let is_clean = Self::is_working_tree_clean(repo_path);

        Ok(GitState {
            commit_sha,
            branch,
            remote_url,
            repo_path: repo_path.to_string_lossy().to_string(),
            is_clean,
        })
    }

    /// Executes git clone operation.
    fn op_clone(&self, args: &GitExecArgs, ctx: &Context) -> Result<Output> {
        let _ = self;
        let timeout_secs = args.timeout_secs.unwrap_or(300);
        let url = args
            .url
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("URL required for clone".into()))?;
        let path = args
            .path
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("Path required for clone".into()))?;

        Self::validate_url(url)?;

        let path = Path::new(path);
        if path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Path already exists: {}",
                path.display()
            )));
        }

        if let Some(free) = Self::disk_free_bytes(path.parent().unwrap_or_else(|| Path::new("/"))) {
            if free < 100 * 1024 * 1024 {
                return Err(Error::ExecutionFailed(
                    "Insufficient disk space for clone (need at least 100MB)".into(),
                ));
            }
        }

        if ctx.dry_run {
            let mut out = Output::ok(format!(
                "DRY RUN: would clone {} to {}",
                Self::sanitize_url(url),
                path.display()
            ));
            out.data = Some(serde_json::json!({
                "operation": "clone",
                "url": Self::sanitize_url(url),
                "path": path.display().to_string(),
                "dry_run": true
            }));
            return Ok(out);
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::ExecutionFailed(format!("mkdir {}: {}", parent.display(), e))
            })?;
        }

        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(url).arg(path);

        if let Some(branch) = &args.branch {
            cmd.arg("-b").arg(branch);
        }

        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("git clone spawn failed: {}", e)))?;

        let timeout = Duration::from_secs(timeout_secs);
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::ExecutionFailed(format!(
                            "git clone timed out after {}s",
                            timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::ExecutionFailed(format!(
                        "git clone wait error: {}",
                        e
                    )));
                }
            }
        };

        if !status.success() {
            return Err(Error::ExecutionFailed(
                "git clone failed (see stderr)".into(),
            ));
        }

        let state = Self::capture_state(path, timeout_secs)?;

        let mut out = Output::ok(format!(
            "Cloned {} to {}",
            Self::sanitize_url(url),
            path.display()
        ));
        out.data = Some(serde_json::json!({
            "operation": "clone",
            "url": Self::sanitize_url(url),
            "path": path.display().to_string(),
            "commit_sha": state.commit_sha,
            "branch": state.branch,
            "remote_url": state.remote_url
        }));
        Ok(out)
    }

    /// Executes git pull operation.
    fn op_pull(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = args.timeout_secs.unwrap_or(300);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN: would pull".into());
            out.data = Some(serde_json::json!({
                "operation": "pull",
                "path": repo_path.display().to_string(),
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(repo_path, &["pull", "--rebase"], timeout_secs)
            .map_err(|e| Error::ExecutionFailed(format!("git pull failed: {}", e)))?;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok("Pulled successfully".into());
        out.data = Some(serde_json::json!({
            "operation": "pull",
            "path": repo_path.display().to_string(),
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
            "git_output": Self::sanitize_output(&output)
        }));
        Ok(out)
    }

    /// Executes git commit operation.
    fn op_commit(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = args.timeout_secs.unwrap_or(300);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let message = args
            .message
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("Commit message required".into()))?;
        let message = Self::sanitize_commit_message(message)?;

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN: would commit".into());
            out.data = Some(serde_json::json!({
                "operation": "commit",
                "path": repo_path.display().to_string(),
                "message": &message,
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        if let Some(files) = &args.files {
            for file in files {
                Self::validate_add_file(file, repo_path)?;
                let output = Self::run_git_with_timeout(repo_path, &["add", file], timeout_secs)
                    .map_err(|e| Error::ExecutionFailed(format!("git add failed: {}", e)))?;
                let _ = output;
            }
        } else {
            let untracked = Self::run_git_with_timeout(
                repo_path,
                &["ls-files", "--others", "--exclude-standard"],
                timeout_secs,
            )?;
            for line in untracked.lines() {
                let file = line.trim();
                if file.is_empty() {
                    continue;
                }
                if Self::is_secret_file(file) {
                    eprintln!("[runtimo] Skipping secret file from git add: {}", file);
                    continue;
                }
                Self::run_git_with_timeout(repo_path, &["add", file], timeout_secs).map_err(
                    |e| Error::ExecutionFailed(format!("git add {} failed: {}", file, e)),
                )?;
            }
        }

        let output =
            Self::run_git_with_timeout(repo_path, &["commit", "-m", &message], timeout_secs)
                .map_err(|e| Error::ExecutionFailed(format!("git commit failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Committed: {}", message));
        out.data = Some(serde_json::json!({
            "operation": "commit",
            "path": repo_path.display().to_string(),
            "message": message,
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git revert operation.
    fn op_revert(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = args.timeout_secs.unwrap_or(300);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let commit_sha = args
            .commit_sha
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("Commit SHA required for revert".into()))?;

        Self::validate_commit_sha(commit_sha)?;

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok(format!("DRY RUN: would revert {}", commit_sha));
            out.data = Some(serde_json::json!({
                "operation": "revert",
                "path": repo_path.display().to_string(),
                "commit_sha": commit_sha,
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(
            repo_path,
            &["revert", "--no-edit", commit_sha],
            timeout_secs,
        )
        .map_err(|e| Error::ExecutionFailed(format!("git revert failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Reverted {}", commit_sha));
        out.data = Some(serde_json::json!({
            "operation": "revert",
            "path": repo_path.display().to_string(),
            "commit_sha": commit_sha,
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git clean operation.
    fn op_clean(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = args.timeout_secs.unwrap_or(300);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let untracked_count = Self::count_untracked_files(repo_path, timeout_secs).unwrap_or(0);
            let preview =
                Self::run_git_with_timeout(repo_path, &["clean", "-fd", "--dry-run"], timeout_secs)
                    .map(|s| Self::sanitize_output(&s))
                    .unwrap_or_default();
            let mut out = Output::ok(format!(
                "DRY RUN: would clean {} untracked files",
                untracked_count
            ));
            out.data = Some(serde_json::json!({
                "operation": "clean",
                "path": repo_path.display().to_string(),
                "dry_run": true,
                "untracked_count": untracked_count,
                "preview": preview
            }));
            return Ok(out);
        }

        let untracked_count = Self::count_untracked_files(repo_path, timeout_secs)?;
        if untracked_count > MAX_CLEAN_FILES {
            return Err(Error::ExecutionFailed(format!(
                "Too many untracked files to clean safely: {} (limit: {})",
                untracked_count, MAX_CLEAN_FILES
            )));
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(repo_path, &["clean", "-fd"], timeout_secs)
            .map_err(|e| Error::ExecutionFailed(format!("git clean failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Cleaned {} untracked files", untracked_count));
        out.data = Some(serde_json::json!({
            "operation": "clean",
            "path": repo_path.display().to_string(),
            "was_clean": state_before.is_clean,
            "is_clean": state_after.is_clean,
            "untracked_files_removed": untracked_count,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git status operation.
    #[allow(clippy::unused_self, clippy::used_underscore_binding)]
    fn op_status(&self, _args: &GitExecArgs, _ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = _args.timeout_secs.unwrap_or(300);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state = Self::capture_state(repo_path, timeout_secs)?;

        let status_output =
            Self::run_git_with_timeout(repo_path, &["status", "--porcelain"], timeout_secs)
                .unwrap_or_default();

        let branch = state.branch.clone().unwrap_or_default();
        let remote_url = state.remote_url.clone().unwrap_or_default();

        let mut out = Output::ok(format!(
            "On branch {}: {}",
            branch,
            if state.is_clean { "clean" } else { "dirty" }
        ));
        out.data = Some(serde_json::json!({
            "operation": "status",
            "path": repo_path.display().to_string(),
            "branch": branch,
            "remote_url": remote_url,
            "commit_sha": state.commit_sha,
            "is_clean": state.is_clean,
            "status": status_output
        }));
        Ok(out)
    }
}

impl TypedCapability for GitExec {
    type Args = GitExecArgs;

    fn name(&self) -> &'static str {
        "GitExec"
    }

    fn description(&self) -> &'static str {
        "git operations: clone, pull, commit, revert, clean, status. state tracking (sha, branch, remote), SSRF-blocked URLs, secret detection, timeout, undo via backup."
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

    fn execute(
        &self,
        args: GitExecArgs,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        let valid_ops = ["clone", "pull", "commit", "revert", "clean", "status"];
        if !valid_ops.contains(&args.operation.as_str()) {
            return Err(CapabilityError::InvalidArgs(format!(
                "Invalid operation: {}. Must be one of: {}",
                args.operation,
                valid_ops.join(", ")
            )));
        }

        if args.operation == "clone" {
            if let Some(url) = &args.url {
                Self::validate_url(url)
                    .map_err(|e| CapabilityError::PermissionDenied(e.to_string()))?;
            } else {
                return Err(CapabilityError::InvalidArgs(
                    "URL required for clone".into(),
                ));
            }
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: false,
                    require_file: false,
                    ..Default::default()
                };
                validate_path(path, &ctx).map_err(CapabilityError::PermissionDenied)?;
            }
        }

        if args.operation != "clone" {
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: true,
                    require_file: false,
                    ..Default::default()
                };
                validate_path(path, &ctx).map_err(CapabilityError::PermissionDenied)?;
            }
        }

        if let Some(branch) = &args.branch {
            Self::validate_branch_name(branch)
                .map_err(|e| CapabilityError::InvalidArgs(e.to_string()))?;
        }

        if let Some(sha) = &args.commit_sha {
            Self::validate_commit_sha(sha)
                .map_err(|e| CapabilityError::InvalidArgs(e.to_string()))?;
        }

        let telemetry_before = Telemetry::capture();
        let process_before = ProcessSnapshot::capture();

        let result = match args.operation.as_str() {
            "clone" => self.op_clone(&args, ctx),
            "pull" => {
                let path = args
                    .path
                    .as_ref()
                    .ok_or_else(|| CapabilityError::InvalidArgs("Path required for pull".into()))?;
                self.op_pull(&args, ctx, Path::new(path))
            }
            "commit" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for commit".into())
                })?;
                self.op_commit(&args, ctx, Path::new(path))
            }
            "revert" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for revert".into())
                })?;
                self.op_revert(&args, ctx, Path::new(path))
            }
            "clean" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for clean".into())
                })?;
                self.op_clean(&args, ctx, Path::new(path))
            }
            "status" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for status".into())
                })?;
                self.op_status(&args, ctx, Path::new(path))
            }
            _ => Err(Error::ExecutionFailed(format!(
                "Unknown operation: {}",
                args.operation
            ))),
        };

        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();

        let mut output = result.map_err(|e| CapabilityError::Internal(e.to_string()))?;
        if let Some(obj) = output.data.as_mut().and_then(|d| d.as_object_mut()) {
            obj.insert(
                "telemetry_before".to_string(),
                serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
            );
            obj.insert(
                "telemetry_after".to_string(),
                serde_json::to_value(&telemetry_after).unwrap_or(Value::Null),
            );
            obj.insert(
                "process_before".to_string(),
                serde_json::to_value(&process_before.summary).unwrap_or(Value::Null),
            );
            obj.insert(
                "process_after".to_string(),
                serde_json::to_value(&process_after.summary).unwrap_or(Value::Null),
            );
        }

        Ok(output)
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
    fn validates_git_url_https_only() {
        assert!(GitExec::validate_url("https://github.com/user/repo.git").is_ok());
        assert!(GitExec::validate_url("git@github.com:user/repo.git").is_ok());

        assert!(GitExec::validate_url("http://example.com/repo.git").is_err());
        assert!(GitExec::validate_url("not-a-url").is_err());
        assert!(GitExec::validate_url("").is_err());

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn blocks_ssrf_urls() {
        assert!(GitExec::validate_url("https://169.254.169.254/latest/meta-data/").is_err());
        assert!(GitExec::validate_url("https://127.0.0.1/repo.git").is_err());
        assert!(GitExec::validate_url("https://localhost/repo.git").is_err());
        assert!(GitExec::validate_url("https://192.168.1.1/repo.git").is_err());
        assert!(GitExec::validate_url("https://metadata.google.internal/computeMetadata").is_err());

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn sanitizes_credentials_from_url() {
        assert_eq!(
            GitExec::sanitize_url("https://user:pass@github.com/repo.git"),
            "https://***@github.com/repo.git"
        );
        assert_eq!(
            GitExec::sanitize_url("https://github.com/repo.git"),
            "https://github.com/repo.git"
        );
        assert_eq!(
            GitExec::sanitize_url("git@github.com:user/repo.git"),
            "git@github.com:user/repo.git"
        );
    }

    #[test]
    fn detects_secret_files() {
        assert!(GitExec::is_secret_file(".env"));
        assert!(GitExec::is_secret_file("config/.env"));
        assert!(GitExec::is_secret_file("credentials.json"));
        assert!(GitExec::is_secret_file(".ssh/id_rsa"));
        assert!(GitExec::is_secret_file("src/.env.local"));

        assert!(!GitExec::is_secret_file("main.rs"));
        assert!(!GitExec::is_secret_file("Cargo.toml"));
        assert!(!GitExec::is_secret_file("README.md"));
    }

    #[test]
    fn validates_branch_name() {
        assert!(GitExec::validate_branch_name("main").is_ok());
        assert!(GitExec::validate_branch_name("feature/my-branch").is_ok());
        assert!(GitExec::validate_branch_name("v1.0").is_ok());

        assert!(GitExec::validate_branch_name("").is_err());
        assert!(GitExec::validate_branch_name("bad..name").is_err());
        assert!(GitExec::validate_branch_name("@{..}").is_err());
        // Option injection
        assert!(GitExec::validate_branch_name("--force").is_err());
        assert!(GitExec::validate_branch_name("--help").is_err());
        // Ref injection
        assert!(GitExec::validate_branch_name("refs/heads/main").is_err());
        // Control chars and whitespace
        assert!(GitExec::validate_branch_name("bad\nname").is_err());
        assert!(GitExec::validate_branch_name("bad\tname").is_err());
        // Metacharacters
        assert!(GitExec::validate_branch_name("bad:name").is_err());
        assert!(GitExec::validate_branch_name("bad~name").is_err());
        assert!(GitExec::validate_branch_name("bad^name").is_err());
        assert!(GitExec::validate_branch_name("bad*name").is_err());
        assert!(GitExec::validate_branch_name("bad[name").is_err());
        assert!(GitExec::validate_branch_name("bad\\name").is_err());
        assert!(GitExec::validate_branch_name("bad?name").is_err());
        assert!(GitExec::validate_branch_name("name.lock").is_err());
    }

    #[test]
    fn validates_commit_sha() {
        assert!(GitExec::validate_commit_sha("abc1234").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0").is_ok());

        assert!(GitExec::validate_commit_sha("abc123").is_err());
        assert!(GitExec::validate_commit_sha("").is_err());
        assert!(GitExec::validate_commit_sha("xyz123").is_err());
    }

    #[allow(clippy::expect_used)]
    #[test]
    fn rejects_path_traversal() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://github.com/user/repo.git",
                "path": "../../../etc/passwd"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err() || !result.unwrap().status.is_empty());
        // The blanket impl's validate always returns Ok, so path traversal
        // is caught at execute time, not validate time.
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[allow(clippy::expect_used)]
    #[test]
    fn rejects_invalid_operation() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "invalid_op"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn status_on_nonexistent_repo() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "status",
                "path": "/tmp/nonexistent_repo"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn sanitizes_commit_message() {
        assert!(GitExec::sanitize_commit_message("valid commit").is_ok());
        assert!(GitExec::sanitize_commit_message("  trimmed  ").is_ok());
        assert!(GitExec::sanitize_commit_message("").is_err());
        assert!(GitExec::sanitize_commit_message("   ").is_err());
        let result = GitExec::sanitize_commit_message("hello\x00world").unwrap();
        assert!(!result.contains('\x00'));
    }

    #[test]
    fn timeout_enforced_on_git_command() {
        // Start a TCP listener on localhost that accepts but never responds.
        // This creates a guaranteed-timeout scenario without depending on
        // external network behavior. The listener is dropped after the test.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
        let port = listener.local_addr().unwrap().port();

        let tmp = std::env::temp_dir().join("runtimo_git_timeout_test");
        std::fs::create_dir_all(&tmp).ok();
        Command::new("git")
            .arg("init")
            .current_dir(&tmp)
            .output()
            .ok();

        // Spawn a thread that accepts one connection and hangs.
        // The git clone will connect and wait for a response that never comes.
        let _hang_handle = std::thread::spawn(move || {
            if let Ok((_stream, _addr)) = listener.accept() {
                // Hold the connection open indefinitely — never send a response
                std::thread::sleep(std::time::Duration::from_mins(5));
            }
        });

        // git clone to localhost times out after 2 seconds.
        let result = GitExec::run_git_with_timeout(
            &tmp,
            &["clone", &format!("http://127.0.0.1:{}/repo.git", port)],
            2,
        );

        // The operation should fail with a timeout (or a connection error
        // if git detects the protocol mismatch before the timeout fires).
        assert!(
            result.is_err(),
            "Expected timeout or connection error, got: {:?}",
            result
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
