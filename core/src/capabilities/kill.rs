//! Kill capability — terminate runaway processes by PID with full audit trail.
//!
//! Kills a process by PID with full telemetry capture and WAL logging.
//! Includes safety checks to prevent killing critical system processes.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::Kill;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let cap = Kill;
//! let result = cap.execute(
//!     &json!({"pid": 12345}),
//!     &Context { dry_run: false, job_id: "test".into(), ..Default::default() }
//! ).unwrap();
//!
//! assert!(result.success);
//! ```

use crate::capability::{Capability, Context, Output};
use crate::processes::ProcessSnapshot;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[cfg(test)]
use std::process::Command;

/// Protected PIDs that cannot be killed (safety guard).
/// Includes init, kthreadd, and the current process (self-protection).
fn protected_pids() -> Vec<u32> {
    let mut pids = vec![1, 2];
    pids.push(std::process::id());
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", std::process::id())) {
        if let Some(ppid_str) = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            if let Ok(ppid) = ppid_str.parse::<u32>() {
                pids.push(ppid);
            }
        }
    }
    pids
}

/// Arguments for the [`Kill`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillArgs {
    /// Process ID to kill.
    pub pid: u32,
    /// Signal to send (default: 15 = SIGTERM).
    pub signal: Option<i32>,
}

/// Capability that terminates a process by PID with full audit logging.
///
/// # Safety
///
/// This capability includes guards to prevent killing critical system processes.
/// Protected PIDs include: 1 (init), 2 (kthreadd).
///
/// # Security
///
/// All kill operations are logged to WAL for audit purposes.
pub struct Kill;

impl Capability for Kill {
    fn name(&self) -> &'static str {
        "Kill"
    }

    /// Returns the JSON Schema for Kill arguments.
    ///
    /// Schema requires `"pid"` integer; `"signal"` and `"force"` are optional.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pid": { "type": "integer", "minimum": 1 },
                "signal": { "type": "integer", "minimum": -64, "maximum": 64 }
            },
            "required": ["pid"]
        })
    }

    fn validate(&self, args: &Value) -> Result<()> {
        let _args: KillArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;
        Ok(())
    }

    fn execute(&self, args: &Value, _ctx: &Context) -> Result<Output> {
        let args: KillArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        // Safety check: protected PIDs (init, kthreadd, self, parent)
        let protected = protected_pids();
        if protected.contains(&args.pid) {
            return Err(Error::ExecutionFailed(format!(
                "PID {} is a protected system process (protected: {:?})",
                args.pid, protected
            )));
        }

        // Capture process snapshot before kill
        let process_before = ProcessSnapshot::capture();
        let process_exists = process_before.processes.iter().any(|p| p.pid == args.pid);

        if !process_exists {
            return Ok(Output {
                success: false,
                data: serde_json::json!({
                    "pid": args.pid,
                    "killed": false,
                    "reason": "Process not found"
                }),
                message: Some(format!("Process {} not found", args.pid)),
            });
        }

        // Get process info before killing
        let process_info: Option<(String, String)> = process_before
            .processes
            .iter()
            .find(|p| p.pid == args.pid)
            .map(|p| (p.command.clone(), p.user.clone()));

        // Determine signal — default to SIGTERM (15) for graceful shutdown
        let signal = args.signal.unwrap_or(15);

        // Execute kill via libc for reliability (avoids shell/PATH issues)
        let kill_result = unsafe { libc::kill(args.pid as libc::pid_t, signal) };
        let success = kill_result == 0;
        let stderr_str = if !success {
            std::io::Error::last_os_error().to_string()
        } else {
            String::new()
        };

        // Delay to let process terminate and be removed from process table
        std::thread::sleep(Duration::from_millis(500));

        // Clear cache to ensure fresh snapshot (cached data would show pre-kill state)
        ProcessSnapshot::clear_cache();

        // Capture process snapshot after kill
        let process_after = ProcessSnapshot::capture();

        // Check if process still exists (zombies count as dead — they've been terminated)
        let process_still_exists = process_after
            .processes
            .iter()
            .any(|p| p.pid == args.pid && !p.stat.starts_with('Z'));

        let message = if success && !process_still_exists {
            format!("Killed process {} (signal {})", args.pid, signal)
        } else if !success {
            format!("Failed to kill process {}: {}", args.pid, stderr_str)
        } else {
            format!("Process {} still exists after signal {}", args.pid, signal)
        };

        Ok(Output {
            success: success && !process_still_exists,
            data: serde_json::json!({
                "pid": args.pid,
                "killed": success && !process_still_exists,
                "signal": signal,
                "command": process_info.as_ref().map(|(cmd, _)| cmd),
                "user": process_info.as_ref().map(|(_, user)| user),
                "stderr": if !success { stderr_str.clone() } else { String::new() },
                "process_before": {
                    "count": process_before.summary.total_processes,
                    "zombies": process_before.summary.zombie_count
                },
                "process_after": {
                    "count": process_after.summary.total_processes,
                    "zombies": process_after.summary.zombie_count
                }
            }),
            message: Some(message),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_kill_schema() {
        let cap = Kill;
        let schema = cap.schema();
        assert_eq!(schema["required"], serde_json::json!(["pid"]));
    }

    #[test]
    fn test_kill_protected_pid() {
        let cap = Kill;
        // PID 1 is protected
        let result = cap.execute(
            &serde_json::json!({ "pid": 1 }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::current_dir().unwrap(),
            },
        );

        // Should fail because PID 1 is protected
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("protected system process"));
    }

    #[test]
    fn test_kill_self_protected() {
        let cap = Kill;
        let self_pid = std::process::id();
        let result = cap.execute(
            &serde_json::json!({ "pid": self_pid }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::current_dir().unwrap(),
            },
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("protected"));
    }

    #[test]
    fn test_kill_nonexistent() {
        let cap = Kill;
        // Use a PID that's very unlikely to exist
        let result = cap
            .execute(
                &serde_json::json!({ "pid": 999999 }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::current_dir().unwrap(),
                },
            )
            .unwrap();

        assert!(!result.success);
        assert!(result.data["killed"].as_bool() == Some(false));
    }

    #[test]
    fn test_kill_actual_process() {
        // Start a long-running process (sleep)
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        // Give it time to start
        thread::sleep(Duration::from_millis(100));

        // Verify process exists before kill
        let pre_check = Command::new("kill").arg("-0").arg(pid.to_string()).output();
        assert!(
            pre_check.unwrap().status.success(),
            "Process should exist before kill"
        );

        // Clear cache so kill sees fresh process list
        ProcessSnapshot::clear_cache();

        // Kill it via the capability using SIGKILL for reliability
        let cap = Kill;
        let result = cap
            .execute(
                &serde_json::json!({ "pid": pid, "signal": 9 }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::current_dir().unwrap(),
                },
            )
            .unwrap();

        // Kill should succeed — process becomes zombie until reaped
        assert!(
            result.data["killed"].as_bool() == Some(true),
            "Kill failed: {:?}",
            result.data
        );
        assert!(
            result.data["signal"].as_i64() == Some(9),
            "Should use SIGKILL"
        );

        // Reap the zombie so it disappears from process table
        let _ = child.wait();

        // Verify process is fully gone after reaping
        let post_check = Command::new("kill").arg("-0").arg(pid.to_string()).output();
        let still_alive = post_check.map(|o| o.status.success()).unwrap_or(false);
        assert!(
            !still_alive,
            "Process {} should be dead after kill and reap",
            pid
        );
    }
}
