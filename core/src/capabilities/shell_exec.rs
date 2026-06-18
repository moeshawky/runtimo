//! ShellExec capability — execute shell commands with full telemetry and audit trail.
//!
//! All commands execute via `sh -c`, providing full shell functionality:
//! - Pipes: `ls | head -5`
//! - Redirects: `echo hello > /tmp/file.txt`
//! - Chaining: `echo first && echo second`
//!
//! # Guardrails (not security)
//!
//! **Threat model:** Agents making mistakes, not attackers.
//! The blocklist catches obvious agent hallucinations/bugs.
//!
//! **What's blocked:**
//! - Filesystem destruction: `rm -rf /`, `rm --recursive`, `rm --no-preserve-root` (all forms blocked)
//! - Shell expansion bypasses: `rm -rf ~` (tilde expansion)
//! - Filesystem creation: `mkfs.*`, `mkswap`
//! - Data destruction: `dd if=/dev/zero`
//! - System commands: `shutdown`, `reboot`, `poweroff`
//! - Disk operations: `fdisk`, `parted`
//! - Permission/ownership changes: `chown`, `chgrp`, `chmod 777 /`
//! - Mount operations: `mount`, `umount`
//! - Firewall manipulation: `iptables`, `nft`
//! - Outbound network tools: `curl`, `wget`, `nc`, `ncat`, `socat`, `ssh`, `scp`, `telnet`
//!   (gated behind `RUNTIMO_ENABLE_NETWORK=1` env var)
//!
//! **PATH sanitization:**
//! ShellExec sets `PATH=/usr/local/bin:/usr/bin:/bin` to limit
//! which executables the command can invoke. Custom binaries
//! in non-standard locations are not resolvable.
//!
//! **What protects you:**
//! - Dangerous command blocklist
//! - Network command gating (opt-in via `RUNTIMO_ENABLE_NETWORK`)
//! - PATH sanitization to known-safe directories
//! - Resource limits (timeout, process isolation)
//! - WAL audit trail (supports undo/recovery)
//!
//! # Features
//!
//! - Timeout enforcement (default 30s, configurable)
//! - Output capture (stdout/stderr, bounded to 10MB)
//! - PID tracking (child PID only; spawned_pids removed from output)
//! - Process group isolation (kills all descendants on timeout)
//! - Telemetry before/after execution
//! - WAL logging for audit trail
//! - Stdin pipe support
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::ShellExec;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let result = ShellExec.execute(
//!     &json!({"cmd": "ls | head -5", "timeout_secs": 10}),
//!     &Context { dry_run: false, job_id: "test".into(), working_dir: std::env::temp_dir() }
//! ).unwrap();
//! ```

use crate::capability::{CapabilityError, Context, Output, TypedCapability};
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

type WaitResult = Result<(ExitStatus, Vec<u8>, Vec<u8>, Vec<u32>)>;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;

/// Input parameters for [`ShellExec::execute`].
///
/// Runs a shell command with an optional timeout and working directory.
/// Dangerous commands (rm -rf /, dd, fork bombs) are rejected before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct ShellExecArgs {
    /// Shell command to execute (e.g. `"ls -la"`, `"cargo build"`).
    #[serde(alias = "command")]
    pub cmd: String,
    /// Maximum seconds before the process is killed (default: 30).
    pub timeout_secs: Option<u64>,
    /// Working directory for the command (default: executor CWD).
    pub cwd: Option<String>,
    /// Data piped to the command's stdin.
    pub stdin: Option<String>,
}

/// Tests whether a command prefix (first whitespace-delimited token) matches
/// any entry in the given list. Avoids false-positives from substrings
/// (e.g. "ssh" in "ssh-agent" is fine when `ssh` is a prefix match but not
/// when it appears mid-word).
fn command_matches(cmd_lower: &str, names: &[&str]) -> bool {
    let first_token = cmd_lower.split_whitespace().next().unwrap_or("");
    // Also check for pipe/chaining context: `echo foo | curl ...` or `true && curl ...`
    for part in cmd_lower.split(['|', '&', ';']) {
        let t = part.trim();
        if names
            .iter()
            .any(|n| t == *n || t.starts_with(&format!("{} ", n)))
        {
            return true;
        }
    }
    names.contains(&first_token)
}

#[must_use]
pub fn is_dangerous_command(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    // rm --no-preserve-root is always blocked — bypasses root safety guard
    if cmd_lower.contains("rm") && cmd_lower.contains("--no-preserve-root") {
        return Some("rm --no-preserve-root is blocked");
    }
    // rm with recursive/destructive flags is always blocked
    // Catches: rm -rf /, rm -fr /*, rm --recursive /, rm -r -f /, etc.
    if cmd_lower.contains("rm")
        && (cmd_lower.contains("-rf")
            || cmd_lower.contains("-fr")
            || cmd_lower.contains("--recursive")
            || cmd_lower.contains(" -r ")
            || cmd_lower.contains(" -f "))
    {
        return Some("recursive rm is blocked");
    }
    if cmd_lower.contains("mkfs") || cmd_lower.contains("mkswap") {
        return Some("filesystem creation commands are blocked");
    }
    if cmd_lower.contains("fdisk") || cmd_lower.contains("parted") {
        return Some("disk partitioning commands are blocked");
    }
    if cmd_lower.contains(" dd ") || cmd_lower.starts_with("dd ") || cmd_lower.contains(" dd") {
        return Some("dd (disk destroyer) is blocked");
    }
    if cmd_lower.contains("shutdown")
        || cmd_lower.contains("reboot")
        || cmd_lower.contains("poweroff")
    {
        return Some("system power commands are blocked");
    }
    // chown/chgrp — ownership changes
    if command_matches(&cmd_lower, &["chown", "chgrp"]) {
        return Some("ownership change commands are blocked");
    }
    // mount/unmount — filesystem mount operations
    if command_matches(&cmd_lower, &["mount", "umount"]) {
        return Some("mount/unmount commands are blocked");
    }
    // iptables/nft — firewall manipulation
    if command_matches(&cmd_lower, &["iptables", "nft"]) {
        return Some("firewall manipulation commands are blocked");
    }
    if cmd_lower.contains("rm")
        && (cmd_lower.contains("-rf")
            || cmd_lower.contains("-fr")
            || cmd_lower.contains("--recursive")
            || cmd_lower.contains(" -r ")
            || cmd_lower.contains(" -f "))
        && (cmd_lower.contains(" / ")
            || cmd_lower.contains("/*")
            || cmd_lower.contains("/dev")
            || cmd_lower.contains("/boot")
            || cmd_lower.contains("/home")
            || cmd_lower.contains("/etc")
            || cmd_lower.contains("/usr")
            || cmd_lower.contains("/var")
            || cmd_lower.contains("/lib")
            || cmd_lower.contains("/opt")
            || cmd_lower.contains("/bin")
            || cmd_lower.contains("/sbin"))
    {
        return Some("rm -rf / --recursive on system directories is blocked");
    }
    if cmd_lower.contains("rm")
        && (cmd_lower.contains("-rf")
            || cmd_lower.contains("-fr")
            || cmd_lower.contains("--recursive")
            || cmd_lower.contains(" -r ")
            || cmd_lower.contains(" -f "))
        && cmd_lower.contains('~')
    {
        return Some("rm with shell expansions is blocked — use explicit paths");
    }
    if cmd_lower.contains("chmod") && cmd_lower.contains("777") && cmd_lower.contains(" /") {
        return Some("chmod 777 / is blocked");
    }
    None
}

/// Tests whether a command invokes a network client.
///
/// Blocked tools: `curl`, `wget`, `nc`/`ncat`/`netcat`, `socat`,
/// `ssh`, `scp`, `telnet`.
///
/// These are only blocked when `RUNTIMO_ENABLE_NETWORK` is not set to `"1"`.
#[must_use]
pub fn is_network_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    command_matches(
        &cmd_lower,
        &[
            "curl", "wget", "nc", "ncat", "netcat", "socat", "ssh", "scp", "telnet",
        ],
    )
}

/// Checks whether outbound network commands are permitted.
///
/// Returns `true` when network tools are allowed (env var set to `"1"`).
#[must_use]
pub fn network_enabled() -> bool {
    std::env::var("RUNTIMO_ENABLE_NETWORK").as_deref() == Ok("1")
}

#[allow(clippy::arithmetic_side_effects)] // -(pgid) negation is safe for valid PIDs
fn wait_with_timeout(child: &mut Child, pgid: u32, timeout_secs: u64) -> WaitResult {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let child_pid = child.id();
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
    let mut last_descendants: Vec<u32>;
    loop {
        if start.elapsed() > timeout {
            // SAFETY: pgid is a valid process group ID from the spawned child; SIGKILL is well-defined;
            // pgid as pid_t may wrap on 32-bit but pgid is always within pid_t range
            #[allow(clippy::cast_possible_wrap)]
            unsafe {
                let _ = libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
            let killed_descendants = get_all_descendants(child_pid);
            let _ = child.wait();
            let _ = stdout_thread.map(|h| h.join().unwrap_or_default());
            let _ = stderr_thread.map(|h| h.join().unwrap_or_default());
            return Err(Error::ExecutionFailed(format!(
                "command timed out after {}s (killed {} descendants)",
                timeout_secs,
                killed_descendants.len()
            )));
        }
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
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(Error::ExecutionFailed(format!("error waiting: {}", e))),
        }
    }
}

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
            if let Ok(output) = std::process::Command::new("pgrep")
                .arg("-P")
                .arg(current.to_string())
                .output()
            {
                if output.status.success() {
                    let pgrep_lines = String::from_utf8_lossy(&output.stdout).to_string();
                    let pgrep_children = pgrep_lines
                        .lines()
                        .filter_map(|s| s.trim().parse::<u32>().ok());
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

/// Capability that executes shell commands with safety guards.
///
/// Commands are run in the executor's process group with a configurable
/// timeout. A blocklist rejects destructive commands (e.g. `rm -rf /`,
/// `dd if=/dev/zero of=/dev/sda`). All executions are logged to the WAL.
#[allow(clippy::exhaustive_structs)]
pub struct ShellExec;

impl TypedCapability for ShellExec {
    type Args = ShellExecArgs;

    fn name(&self) -> &'static str {
        "ShellExec"
    }
    fn description(&self) -> &'static str {
        "execute shell command via sh -c with timeout, audit trail, and PID tracking. blocks: mkfs, fdisk, dd, shutdown, rm -rf /, chown, mount, iptables, network tools (opt-in)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Command to execute via sh -c" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
                "cwd": { "type": "string" },
                "stdin": { "type": "string" }
            },
            "required": ["cmd"]
        })
    }
    fn execute(&self, args: ShellExecArgs, ctx: &Context) -> std::result::Result<Output, CapabilityError> {
        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN".into());
            out.data = Some(serde_json::json!({ "cmd": &args.cmd, "dry_run": true }));
            return Ok(out);
        }
        let timeout = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if let Some(reason) = is_dangerous_command(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(format!(
                "dangerous command blocked: {}",
                reason
            )));
        }
        if !network_enabled() && is_network_command(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(
                "network commands blocked — set RUNTIMO_ENABLE_NETWORK=1 to enable".into(),
            ));
        }
        let mut cmd = Command::new("sh");
        // PATH sanitization: limit executable resolution to trusted system dirs.
        // This is defense-in-depth — the blocklist catches known-dangerous
        // commands, but this prevents invocation of custom binaries in
        // non-standard locations.
        cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        cmd.arg("-c").arg(&args.cmd);
        if let Some(cwd) = &args.cwd {
            let path_ctx = PathContext {
                require_exists: true,
                require_file: false,
                ..Default::default()
            };
            let cwd_path = validate_path(cwd, &path_ctx)
                .map_err(|e| CapabilityError::PermissionDenied(format!("invalid cwd: {}", e)))?;
            cmd.current_dir(cwd_path);
        }
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
            .map_err(|e| CapabilityError::Io(std::io::Error::other(
                format!("failed to spawn: {}", e),
            )))?;
        let child_pid = child.id();
        let pgid = child_pid;
        if let Some(ref stdin_content) = args.stdin {
            if stdin_content.len() > MAX_STDIN_BYTES {
                return Err(CapabilityError::InvalidArgs("stdin too large".into()));
            }
            if let Some(mut stdin_pipe) = child.stdin.take() {
                let _ = stdin_pipe.write_all(stdin_content.as_bytes());
            }
        }
        let (exit_status, stdout, stderr, _descendants) =
            wait_with_timeout(&mut child, pgid, timeout)
                .map_err(|e| CapabilityError::Internal(e.to_string()))?;
        let stdout_str = String::from_utf8_lossy(&stdout).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr).to_string();
        let success = exit_status.success();

        let mut out = if success {
            Output::ok("completed".into())
        } else {
            Output::error(
                format!("exit code {}", exit_status.code().unwrap_or(-1)),
                format!("exit code {}", exit_status.code().unwrap_or(-1)),
            )
        };
        out.data = Some(serde_json::json!({ "cmd": &args.cmd, "stdout": stdout_str, "stderr": stderr_str, "exit_code": exit_status.code().unwrap_or(-1), "pid": child_pid, "timeout_secs": timeout, "timed_out": exit_status.code().is_none(), "truncated": stdout.len() >= MAX_OUTPUT_BYTES || stderr.len() >= MAX_OUTPUT_BYTES }));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use std::time::Instant;
    #[test]
    fn executes_uptime() {
        let r = Capability::execute(&ShellExec, &serde_json::json!({"cmd": "uptime"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
    }
    #[test]
    fn pipes_work() {
        let r = Capability::execute(&ShellExec, &serde_json::json!({"cmd": "echo hi | cat"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
        assert!(r.data.as_ref().unwrap()["stdout"].as_str().unwrap().contains("hi"));
    }
    #[test]
    fn chaining_works() {
        let r = Capability::execute(&ShellExec, &serde_json::json!({"cmd": "echo a && echo b"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
    }
    #[test]
    fn blocks_dangerous() {
        assert!(Capability::execute(&ShellExec, &serde_json::json!({"cmd": "mkfs"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir()
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_recursive_flag() {
        // rm --recursive (long form) should be caught like -rf
        assert!(Capability::execute(&ShellExec, &serde_json::json!({"cmd": "rm --recursive /home"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir()
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_rm_rf_root() {
        // rm -rf / should always be blocked regardless of context
        assert!(Capability::execute(&ShellExec, &serde_json::json!({"cmd": "rm -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir()
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_rm_no_preserve_root() {
        assert!(Capability::execute(&ShellExec, &serde_json::json!({"cmd": "rm --no-preserve-root -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir()
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_ownership_commands() {
        for cmd in &["chown root /tmp/x", "chgrp staff /tmp/x"] {
            assert!(
                Capability::execute(&ShellExec, &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir()
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_mount_commands() {
        for cmd in &["mount /dev/sda1 /mnt", "umount /mnt"] {
            assert!(
                Capability::execute(&ShellExec, &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir()
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_firewall_commands() {
        for cmd in &["iptables -L", "nft list ruleset"] {
            assert!(
                Capability::execute(&ShellExec, &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir()
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_network_commands_by_default() {
        // Ensure RUNTIMO_ENABLE_NETWORK is not set
        std::env::remove_var("RUNTIMO_ENABLE_NETWORK");
        for cmd in &[
            "curl http://example.com",
            "wget http://example.com",
            "nc example.com 80",
        ] {
            assert!(
                Capability::execute(&ShellExec, &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir()
                    }
                )
                .is_err(),
                "should block network cmd: {}",
                cmd
            );
        }
    }
    #[test]
    fn allows_network_commands_when_enabled() {
        std::env::set_var("RUNTIMO_ENABLE_NETWORK", "1");
        // curl --version should work (non-destructive)
        let r = Capability::execute(&ShellExec, &serde_json::json!({"cmd": "curl --version"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        std::env::remove_var("RUNTIMO_ENABLE_NETWORK");
        // May fail if curl not installed, but should NOT fail with "network commands blocked"
        match r {
            Ok(o) => assert_eq!(o.status, "ok", "curl --version should succeed when enabled"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("network commands blocked"),
                    "should NOT block network when RUNTIMO_ENABLE_NETWORK=1, got: {}",
                    msg
                );
            }
        }
    }
    #[test]
    fn enforces_timeout() {
        let s = Instant::now();
        assert!(Capability::execute(&ShellExec, &serde_json::json!({"cmd": "sleep 5", "timeout_secs": 1}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir()
            }
        )
        .is_err());
        assert!(s.elapsed().as_secs() < 3);
    }
}
