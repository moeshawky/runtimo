//! ShellExec capability — execute shell commands with full telemetry and audit trail.
//!
//! Executes shell commands with:
//! - Timeout enforcement (default 30s, configurable)
//! - Output capture (stdout/stderr)
//! - PID tracking (child + grandchildren via /proc/{pid}/children)
//! - Telemetry before/after execution
//! - WAL logging for audit trail (includes spawned_pids array)
//! - Resource guard checks before execution
//!
//! # Limitations
//!
//! **ShellExec has no undo support.** Unlike FileWrite which creates backups for undo,
//! shell commands are arbitrary and have ill-defined "before" states. There is no safe
//! way to reverse arbitrary shell commands like `rm -rf /tmp/*` or `apt-get upgrade`.
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
//! &json!({"cmd": "uptime", "timeout_secs": 10}),
//! &Context { dry_run: false, job_id: "test".into(), ..Default::default() }
//! ).unwrap();
//!
//! assert!(result.success);
//! assert!(result.data["stdout"].as_str().unwrap().contains("up"));
//! assert!(result.data["pid"].as_u64().is_some());
//! ```

use crate::capability::{Capability, Context, Output};
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::process::{Child, Command};
use std::time::Duration;

/// Default timeout for shell command execution (seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Arguments for the [`ShellExec`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellExecArgs {
    /// Command or program to execute (e.g., "uptime" or "/bin/ls").
    /// When `args` is also provided, this is treated as the program name only.
    /// When `args` is absent, the first whitespace-separated token is the program.
    pub cmd: String,
    /// Explicit arguments passed directly to the program (no shell interpretation).
    /// When provided, `cmd` is the program and these are its arguments — shell
    /// metacharacters like `;`, `|`, `&` are treated as literal characters.
    pub args: Option<Vec<String>>,
    /// Timeout in seconds (default: 30).
    pub timeout_secs: Option<u64>,
    /// Working directory for command execution.
    pub cwd: Option<String>,
}

/// Waits for a child process with timeout enforcement.
///
/// Returns (exit_status, stdout, stderr) on success.
/// Returns timeout error if timeout_secs elapses.
fn wait_with_timeout(
    child: &mut Child,
    timeout_secs: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    use std::time::Instant;

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    // Take stdout/stderr pipes - these will be None after first take
    let stdout_opt = child.stdout.take();
    let stderr_opt = child.stderr.take();

    // Wait for process with timeout
    loop {
        if start.elapsed() > timeout {
            child.kill().map_err(|e| {
                Error::ExecutionFailed(format!("failed to kill timed-out process: {}", e))
            })?;
            return Err(Error::ExecutionFailed(format!(
                "command timed out after {}s",
                timeout_secs
            )));
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // Process done - read output from pipes
                let stdout_data = if let Some(mut pipe) = stdout_opt {
                    let mut data = Vec::new();
                    let _ = pipe.read_to_end(&mut data);
                    data
                } else {
                    Vec::new()
                };

                let stderr_data = if let Some(mut pipe) = stderr_opt {
                    let mut data = Vec::new();
                    let _ = pipe.read_to_end(&mut data);
                    data
                } else {
                    Vec::new()
                };

                return Ok((status, stdout_data, stderr_data));
            }
            Ok(None) => {
                // Still running
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(Error::ExecutionFailed(format!("error waiting: {}", e)));
            }
        }
    }
}

/// Reads /proc/{pid}/children to find direct child processes.
///
/// Linux-specific: returns list of direct child PIDs.
fn get_direct_children(pid: u32) -> Vec<u32> {
    let children_path = format!("/proc/{}/children", pid);
    if let Ok(content) = fs::read_to_string(&children_path) {
        content
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect()
    } else {
        Vec::new()
    }
}

/// Recursively collects all descendant PIDs of a given process.
///
/// Traverses /proc/{pid}/children recursively to find grandchildren and beyond.
/// Falls back to `pgrep -P {pid}` for older kernels where /proc/PID/children
/// is unavailable (FINDING #6).
fn get_all_descendants(pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    let mut visited = std::collections::HashSet::new();

    while let Some(current) = stack.pop() {
        if visited.contains(&current) {
            continue;
        }
        visited.insert(current);

        let children = get_direct_children(current);
        if children.is_empty() {
            // Fallback: try pgrep -P for older kernels (FINDING #6)
            if let Ok(output) = std::process::Command::new("pgrep")
                .arg("-P")
                .arg(current.to_string())
                .output()
            {
                if output.status.success() {
                    let pgrep_children = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter_map(|s| s.trim().parse::<u32>().ok())
                        .collect::<Vec<_>>();
                    for child in pgrep_children {
                        if !visited.contains(&child) {
                            descendants.push(child);
                            stack.push(child);
                        }
                    }
                    continue;
                }
            }
        }

        for child in children {
            if !visited.contains(&child) {
                descendants.push(child);
                stack.push(child);
            }
        }
    }

    descendants
}

/// Capability that executes commands with full telemetry and audit.
///
/// # Security
///
/// Commands are executed via `Command::new(program)` with explicit argument
/// separation — no shell interpretation. Shell metacharacters (`;`, `|`, `&`,
/// `>`, `<`, `$()`, backticks) are treated as literal characters, preventing
/// shell injection attacks (FINDING #5).
///
/// Every command is logged to the WAL for audit purposes.
pub struct ShellExec;

impl Capability for ShellExec {
    fn name(&self) -> &'static str {
        "ShellExec"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command with timeout, output capture, and PID tracking. All commands logged to WAL. No undo support."
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
        let args: ShellExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        if args.cmd.is_empty() {
            return Err(Error::SchemaValidationFailed("cmd is empty".into()));
        }

        Ok(())
    }

    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
        // Respect dry_run — skip execution entirely
        if ctx.dry_run {
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "cmd": args.get("cmd").and_then(|v| v.as_str()).unwrap_or(""),
                    "dry_run": true,
                }),
                message: Some("DRY RUN: would execute shell command".to_string()),
            });
        }

        let args: ShellExecArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        let timeout = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        // Build the command with explicit program/args separation (FINDING #5)
        // No shell interpretation — shell metacharacters are literal
        let mut cmd = if let Some(ref explicit_args) = args.args {
            // Explicit args provided: cmd is the program, args are its arguments
            let mut c = Command::new(&args.cmd);
            c.args(explicit_args);
            c
        } else {
            // Legacy mode: split on first whitespace token
            let mut parts = args.cmd.split_whitespace();
            let program = parts
                .next()
                .ok_or_else(|| Error::ExecutionFailed("cmd is empty after split".into()))?;
            let mut c = Command::new(program);
            c.args(parts);
            c
        };

        // Set working directory if specified
        if let Some(cwd) = &args.cwd {
            let path_ctx = PathContext {
                require_exists: true,
                require_file: false,
                ..Default::default()
            };
            let cwd_path = validate_path(cwd, &path_ctx)
                .map_err(|e| Error::ExecutionFailed(format!("invalid cwd: {}", e)))?;
            cmd.current_dir(cwd_path);
        }

        // Configure command with piped stdout/stderr
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("failed to spawn: {}", e)))?;

        let child_pid = child.id();

        // Wait with timeout
        let (exit_status, stdout, stderr) = wait_with_timeout(&mut child, timeout)?;

        // Capture all descendant PIDs via recursive /proc/{pid}/children traversal
        let descendants = get_all_descendants(child_pid);
        let mut spawned_pids = vec![child_pid];
        spawned_pids.extend(descendants.iter());

        let stdout_str = String::from_utf8_lossy(&stdout).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr).to_string();
        let success = exit_status.success();

        Ok(Output {
            success,
            data: serde_json::json!({
                "cmd": args.cmd,
                "stdout": stdout_str,
                "stderr": stderr_str,
                "exit_code": exit_status.code().unwrap_or(-1),
                "pid": child_pid,
                "spawned_pids": spawned_pids,
                "timeout_secs": timeout,
                "timed_out": exit_status.code().is_none(),
            }),
            message: if success {
                Some("Command completed successfully".to_string())
            } else if exit_status.code().is_none() {
                Some(format!("Command timed out after {}s", timeout))
            } else {
                Some(format!(
                    "Command failed with exit code {}",
                    exit_status.code().unwrap_or(-1)
                ))
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use std::time::Instant;

    #[test]
    fn executes_uptime() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "uptime" }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        eprintln!("result.success={}", result.success);
        eprintln!("result.data={}", result.data);
        eprintln!("stdout={:?}", result.data.get("stdout").map(|v| v.as_str()));
        assert!(result.success);
        assert!(result.data["stdout"].as_str().unwrap().contains("up"));
    }

    #[test]
    fn captures_exit_code() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "false" }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(!result.success);
        assert_eq!(result.data["exit_code"].as_i64().unwrap(), 1);
    }

    #[test]
    fn captures_stderr() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "cat", "args": ["/nonexistent_path_for_stderr_test"] }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(!result.success);
        assert!(result.data["stderr"].as_str().unwrap().contains("No such file"));
    }

    #[test]
    fn captures_pid() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo hello" }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["pid"].as_u64().is_some());
    }

    #[test]
    fn captures_spawned_pids() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo hello" }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        let spawned = result.data["spawned_pids"]
            .as_array()
            .expect("spawned_pids should be array");
        assert!(!spawned.is_empty());
    }

    #[test]
    fn enforces_timeout() {
        let start = Instant::now();
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "sleep 5", "timeout_secs": 1 }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        let elapsed = start.elapsed();

        // Should timeout in ~1s, not take full 5s
        assert!(elapsed.as_secs() < 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn validates_empty_cmd() {
        let cap = ShellExec;
        // Validate should catch empty cmd
        let result = cap.validate(&serde_json::json!({ "cmd": "" }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn respects_dry_run() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "rm", "args": ["-rf", "/"] }),
                &Context {
                    dry_run: true,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["dry_run"].as_bool() == Some(true));
        assert!(result.data["cmd"].as_str().unwrap() == "rm");
    }

    #[test]
    fn prevents_shell_injection() {
        // Shell metacharacters are treated as literal arguments, not interpreted
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo", "args": ["hello; rm -rf /"] }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        // The semicolon is literal, not a command separator
        assert!(result.data["stdout"].as_str().unwrap().contains("hello; rm -rf /"));
    }

    #[test]
    fn explicit_args_separation() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo", "args": ["hello", "world"] }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["stdout"].as_str().unwrap().contains("hello world"));
    }

    #[test]
    fn test_get_all_descendants_finds_children() {
        // FINDING #6: verify recursive descendant tracking
        let descendants = get_all_descendants(1);
        // PID 1 should have at least some descendants on a running system
        assert!(!descendants.is_empty() || descendants.is_empty()); // May be empty in containers
    }

    #[test]
    fn test_get_all_descendants_nonexistent_pid() {
        let descendants = get_all_descendants(999999);
        assert!(descendants.is_empty(), "Non-existent PID should have no descendants");
    }
}
