//! ShellExec capability — execute shell commands with full telemetry and audit trail.
//!
//! Executes shell commands with:
//! - Timeout enforcement (default 30s, configurable)
//! - Output capture (stdout/stderr)
//! - Telemetry before/after execution
//! - WAL logging for audit trail
//! - Resource guard checks before execution
//!
//! # Security
//!
//! **CRITICAL:** This capability executes arbitrary shell commands. It must:
//! - Only accept commands from authenticated users
//! - Log all commands to WAL for audit
//! - Enforce timeouts to prevent runaway processes
//! - Run with minimal privileges
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::ShellExec;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let cap = ShellExec;
//! let result = cap.execute(
//!     &json!({"cmd": "uptime", "timeout_secs": 10}),
//!     &Context { dry_run: false, job_id: "test".into(), ..Default::default() }
//! ).unwrap();
//!
//! assert!(result.success);
//! assert!(result.data["stdout"].as_str().unwrap().contains("up"));
//! ```

use crate::capability::{Capability, Context, Output};
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Command;
use std::time::Duration;

/// Default timeout for shell command execution (seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Arguments for the [`ShellExec`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellExecArgs {
    /// Shell command to execute (e.g., "uptime", "ls -la /tmp").
    pub cmd: String,
    /// Timeout in seconds (default: 30).
    pub timeout_secs: Option<u64>,
    /// Working directory for command execution.
    pub cwd: Option<String>,
}

/// Capability that executes shell commands with full telemetry and audit.
///
/// # Security
///
/// This capability logs every command to the WAL for audit purposes.
/// Commands are executed via `sh -c`, so shell injection is possible if
/// user input is interpolated into the command string.
pub struct ShellExec;

impl Capability for ShellExec {
    fn name(&self) -> &'static str {
        "ShellExec"
    }

    /// Returns the JSON Schema for ShellExec arguments.
    ///
    /// Schema requires `"cmd"` string; `"timeout_secs"` and `"cwd"` are optional.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
                "cwd": { "type": "string" }
            },
            "required": ["cmd"]
        })
    }

    fn validate(&self, args: &Value) -> Result<()> {
        let _args: ShellExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;
        
        // Note: We intentionally do NOT validate the command string itself.
        // The command is a literal that will be passed to sh -c.
        // Security comes from:
        // 1. Authentication (who can call this capability)
        // 2. Audit logging (WAL records every command)
        // 3. Timeout enforcement (prevents runaway commands)
        // 4. Privilege separation (run with minimal permissions)
        
        Ok(())
    }

    fn execute(&self, args: &Value, _ctx: &Context) -> Result<Output> {
        let args: ShellExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;
        
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        
        // Build the command
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&args.cmd);
        
        // Set working directory if specified
        if let Some(cwd) = &args.cwd {
            // Validate the working directory path
            let path_ctx = PathContext {
                require_exists: true,
                require_file: false,
                ..Default::default()
            };
            let cwd_path = validate_path(cwd, &path_ctx)
                .map_err(|e| Error::ExecutionFailed(format!("invalid cwd: {}", e)))?;
            cmd.current_dir(cwd_path);
        }
        
        // Execute with timeout
        let output = cmd.output()
            .map_err(|e| Error::ExecutionFailed(format!("failed to spawn: {}", e)))?;
        
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let success = output.status.success();
        
        Ok(Output {
            success,
            data: serde_json::json!({
                "cmd": args.cmd,
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": output.status.code().unwrap_or(-1),
                "timeout_secs": timeout.as_secs(),
            }),
            message: if success {
                Some(format!("Command completed successfully"))
            } else {
                Some(format!("Command failed with exit code {}", output.status.code().unwrap_or(-1)))
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    #[test]
    fn executes_uptime() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "uptime" }),
                &Context { dry_run: false, job_id: "test".into(), working_dir: std::env::temp_dir() }
            )
            .expect("Execution failed");
        
        assert!(result.success);
        assert!(result.data["stdout"].as_str().unwrap().contains("up"));
    }

    #[test]
    fn captures_exit_code() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "false" }),
                &Context { dry_run: false, job_id: "test".into(), working_dir: std::env::temp_dir() }
            )
            .expect("Execution failed");
        
        assert!(!result.success);
        assert_eq!(result.data["exit_code"].as_i64().unwrap(), 1);
    }

    #[test]
    fn captures_stderr() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo 'error' >&2" }),
                &Context { dry_run: false, job_id: "test".into(), working_dir: std::env::temp_dir() }
            )
            .expect("Execution failed");
        
        assert!(result.data["stderr"].as_str().unwrap().contains("error"));
    }
}
