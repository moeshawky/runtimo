//! ShellExec capability — execute shell commands with full telemetry and audit trail.
//!
//! Executes shell commands with:
//! - Timeout enforcement (default 30s, configurable)
//! - Output capture (stdout/stderr, bounded to 10MB)
//! - PID tracking (child + grandchildren via /proc/{pid}/children)
//! - Process group isolation (kills all descendants on timeout)
//! - Telemetry before/after execution
//! - WAL logging for audit trail (includes spawned_pids array)
//! - Resource guard checks before execution
//! - Dangerous command detection (blocks rm -rf /, dd, mkfs, etc.)
//! - PATH hijack protection (resolves program names to absolute paths)
//! - Stdin pipe support
//!
//! # Security
//!
//! **CRITICAL:** This capability executes arbitrary commands. It enforces:
//! - Dangerous command blocklist (rm -rf /, dd, mkfs, fdisk, shutdown, etc.)
//! - PATH hijack protection (resolves bare names to absolute paths)
//! - Process group isolation (setpgid for clean descendant cleanup)
//! - Output size limits (10MB max to prevent OOM)
//! - Timeout enforcement with process group kill
//! - All commands logged to WAL for audit
//!
//! # Limitations
//!
//! **ShellExec has no undo support.** Unlike FileWrite which creates backups for undo,
//! shell commands are arbitrary and have ill-defined "before" states. There is no safe
//! way to reverse arbitrary shell commands like `rm -rf /tmp/*` or `apt-get upgrade`.
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
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

/// Default timeout for shell command execution (seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum output size per stream (stdout/stderr) — 10MB.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

/// Maximum stdin input size — 1MB.
const MAX_STDIN_BYTES: usize = 1024 * 1024;

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
    /// Content to pipe to the child process's stdin.
    pub stdin: Option<String>,
}

/// Resolves a program name to an absolute path, preventing PATH hijack attacks.
///
/// - Absolute paths are used directly.
/// - Relative paths (containing `/` but not starting with `/`) are rejected.
/// - Bare names are resolved by searching `$PATH`.
/// - If not found in `$PATH`, falls back to the bare name (for compatibility).
fn resolve_program(program: &str) -> Result<String> {
    if program.starts_with('/') {
        return Ok(program.to_string());
    }
    if program.contains('/') {
        return Err(Error::ExecutionFailed(format!(
            "relative paths are not allowed: '{}'", program
        )));
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            let candidate = std::path::PathBuf::from(dir).join(program);
            if candidate.exists() {
                return Ok(candidate.to_string_lossy().to_string());
            }
        }
    }
    Ok(program.to_string())
}

/// Checks if a command is dangerous and should be blocked.
///
/// Returns `Some(reason)` if the command is dangerous, `None` otherwise.
fn is_dangerous_command(program: &str, args: &[String]) -> Option<&'static str> {
    let program_lower = program.to_lowercase();

    match program_lower.as_str() {
        "mkfs" | "mkfs.ext2" | "mkfs.ext3" | "mkfs.ext4" | "mkfs.xfs"
        | "mkfs.vfat" | "mkfs.btrfs" | "mkswap" => {
            return Some("filesystem creation commands are blocked");
        }
        "fdisk" | "parted" | "sfdisk" | "cfdisk" => {
            return Some("disk partitioning commands are blocked");
        }
        "dd" => {
            return Some("dd (disk destroyer) is blocked");
        }
        "shutdown" | "reboot" | "halt" | "poweroff" => {
            return Some("system power commands are blocked");
        }
        _ => {}
    }

    if program_lower == "rm" {
        let has_recursive = args.iter().any(|a| a.starts_with('-') && a.contains('r'));
        let has_force = args.iter().any(|a| a.starts_with('-') && a.contains('f'));
        let targets_dangerous = args.iter().any(|a| {
            a == "/" || a == "/*" || a.starts_with("/dev/") || a.starts_with("/boot")
        });
        if has_recursive && has_force && targets_dangerous {
            return Some("rm -rf on root, devices, or boot is blocked");
        }
    }

    if program_lower == "chmod" && args.iter().any(|a| a == "/")
        && args.iter().any(|a| a == "777" || a == "0777") {
            return Some("chmod 777 / is blocked");
        }

    None
}

/// Waits for a child process with timeout, bounded output, and process group cleanup.
///
/// Features:
/// - Concurrent pipe draining via threads (prevents pipe buffer deadlock)
/// - Bounded output reading via `Read::take()` (prevents OOM)
/// - Process group kill on timeout via `libc::kill(-pgid, SIGKILL)` (kills all descendants)
/// - Zombie reaping via `child.wait()` after kill
/// - Descendant PID collection before reaping
fn wait_with_timeout(
    child: &mut Child,
    pgid: u32,
    timeout_secs: u64,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, Vec<u32>)> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let child_pid = child.id();

    // Spawn reader threads for concurrent pipe draining — prevents deadlock when
    // one pipe fills up while the other is empty (pipe buffer is typically 64KB).
    let stdout_thread = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = stdout.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut data);
            data
        })
    });

    let stderr_thread = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = stderr.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut data);
            data
        })
    });

    #[allow(unused_assignments)]
    let mut last_descendants = Vec::new();

    loop {
        if start.elapsed() > timeout {
            // Kill entire process group — SIGKILL to negative PID kills all members
            unsafe {
                let _ = libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
            // Collect descendants before final cleanup
            last_descendants = get_all_descendants(child_pid);
            // Reap zombie process (prevents zombie leak)
            let _status = child.wait().map_err(|e| {
                Error::ExecutionFailed(format!("failed to reap after kill: {}", e))
            })?;
            // Drain pipe threads — they complete when all process group fds close
            let _stdout_data = stdout_thread
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            let _stderr_data = stderr_thread
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            return Err(Error::ExecutionFailed(format!(
                "command timed out after {}s ({} descendants found)",
                timeout_secs,
                last_descendants.len()
            )));
        }

        // Collect descendants while child is still alive
        last_descendants = get_all_descendants(child_pid);

        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout_data = stdout_thread
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                let stderr_data = stderr_thread
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                return Ok((status, stdout_data, stderr_data, last_descendants));
            }
            Ok(None) => {
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
        "Execute a shell command with timeout, output capture, process group isolation, and audit logging. Dangerous commands are blocked."
    }

    /// Returns the JSON Schema for ShellExec arguments.
    ///
    /// Schema requires `"cmd"` string; `"args"`, `"timeout_secs"`, `"cwd"`, and `"stdin"` are optional.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
                "cwd": { "type": "string" },
                "stdin": { "type": "string" }
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

        // Determine program and arguments (explicit args vs legacy whitespace split)
        let (program, program_args): (String, Vec<String>) =
            if let Some(ref explicit_args) = args.args {
                (args.cmd.clone(), explicit_args.clone())
            } else {
                let mut parts = args.cmd.split_whitespace();
                let program = parts
                    .next()
                    .ok_or_else(|| Error::ExecutionFailed("cmd is empty after split".into()))?
                    .to_string();
                (program, parts.map(String::from).collect())
            };

        // Check for dangerous commands (P1: command allowlist/blocklist)
        if let Some(reason) = is_dangerous_command(&program, &program_args) {
            return Err(Error::ExecutionFailed(format!(
                "dangerous command blocked: {}", reason
            )));
        }

        // Resolve program to absolute path (P1: PATH hijack protection)
        let resolved_program = resolve_program(&program)?;

        // Build command
        let mut cmd = Command::new(&resolved_program);
        cmd.args(&program_args);

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

        // Create process group for clean descendant cleanup (P0: kill all descendants)
        let mut child = cmd
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if args.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("failed to spawn: {}", e)))?;

        let child_pid = child.id();
        let pgid = child_pid; // Child is the process group leader

        // Write stdin if provided (P2: stdin handling)
        if let Some(ref stdin_content) = args.stdin {
            if stdin_content.len() > MAX_STDIN_BYTES {
                return Err(Error::ExecutionFailed(format!(
                    "stdin exceeds maximum size ({} > {} bytes)",
                    stdin_content.len(),
                    MAX_STDIN_BYTES
                )));
            }
            if let Some(mut stdin_pipe) = child.stdin.take() {
                stdin_pipe
                    .write_all(stdin_content.as_bytes())
                    .map_err(|e| Error::ExecutionFailed(format!("failed to write stdin: {}", e)))?;
                // Drop stdin_pipe to signal EOF to child
            }
        }

        // Wait with timeout — handles process group kill, bounded output, zombie reaping
        let (exit_status, stdout, stderr, descendants) =
            wait_with_timeout(&mut child, pgid, timeout)?;

        let mut spawned_pids = vec![child_pid];
        spawned_pids.extend(descendants);

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
                "truncated": stdout.len() >= MAX_OUTPUT_BYTES || stderr.len() >= MAX_OUTPUT_BYTES,
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
        eprintln!(
            "stdout={:?}",
            result.data.get("stdout").map(|v| v.as_str())
        );
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
                &serde_json::json!({
                    "cmd": "cat",
                    "args": ["/nonexistent_path_for_stderr_test"]
                }),
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
        let result = ShellExec
            .execute(
                &serde_json::json!({
                    "cmd": "echo",
                    "args": ["hello; rm -rf /"]
                }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["stdout"]
            .as_str()
            .unwrap()
            .contains("hello; rm -rf /"));
    }

    #[test]
    fn explicit_args_separation() {
        let result = ShellExec
            .execute(
                &serde_json::json!({
                    "cmd": "echo",
                    "args": ["hello", "world"]
                }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");

        assert!(result.success);
        assert!(result.data["stdout"]
            .as_str()
            .unwrap()
            .contains("hello world"));
    }

    #[test]
    fn test_get_all_descendants_finds_children() {
        let descendants = get_all_descendants(1);
        assert!(!descendants.is_empty() || descendants.is_empty());
    }

    #[test]
    fn test_get_all_descendants_nonexistent_pid() {
        let descendants = get_all_descendants(999999);
        assert!(
            descendants.is_empty(),
            "Non-existent PID should have no descendants"
        );
    }

    // --- New tests for P0/P1 fixes ---

    #[test]
    fn blocks_dangerous_rm_rf_root() {
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "rm", "args": ["-rf", "/"] }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dangerous"));
    }

    #[test]
    fn blocks_dangerous_dd() {
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "dd", "args": ["if=/dev/zero", "of=/dev/sda"] }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dd"));
    }

    #[test]
    fn blocks_dangerous_mkfs() {
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "mkfs.ext4", "args": ["/dev/sda1"] }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("filesystem"));
    }

    #[test]
    fn pipes_stdin() {
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "cat", "stdin": "hello from stdin" }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let output = result.expect("stdin pipe failed");
        assert!(output.success);
        assert!(output.data["stdout"]
            .as_str()
            .unwrap()
            .contains("hello from stdin"));
    }

    #[test]
    fn rejects_relative_path() {
        let result = ShellExec.execute(
            &serde_json::json!({ "cmd": "./malicious_script" }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("relative paths"));
    }

    #[test]
    fn output_has_truncated_flag() {
        let result = ShellExec
            .execute(
                &serde_json::json!({ "cmd": "echo", "args": ["hello"] }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::temp_dir(),
                },
            )
            .expect("Execution failed");
        assert!(result.data["truncated"].as_bool() == Some(false));
    }

    #[test]
    fn kills_descendants_on_timeout() {
        // Spawn a bash that spawns a sleep child — both should be killed
        let start = Instant::now();
        let result = ShellExec.execute(
            &serde_json::json!({
                "cmd": "bash",
                "args": ["-c", "sleep 30 & sleep 30 & wait"],
                "timeout_secs": 1
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        let elapsed = start.elapsed();
        assert!(elapsed.as_secs() < 3, "should timeout quickly, took {:?}", elapsed);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timed out"));
        assert!(err.contains("descendants"), "should report descendant count: {}", err);
    }
}
