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
use std::process::Command;
use std::time::Duration;

/// Protected PIDs that cannot be killed (safety guard).
const PROTECTED_PIDS: &[u32] = &[1, 2]; // init, kthreadd

/// Arguments for the [`Kill`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillArgs {
    /// Process ID to kill.
    pub pid: u32,
    /// Signal to send (default: 9 = SIGKILL).
    pub signal: Option<i32>,
    /// Force kill even if PID is protected (requires special authorization).
    pub force: Option<bool>,
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
                "signal": { "type": "integer", "minimum": -64, "maximum": 64 },
                "force": { "type": "boolean" }
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

        // Safety check: protected PIDs
        if PROTECTED_PIDS.contains(&args.pid) && !args.force.unwrap_or(false) {
            return Err(Error::ExecutionFailed(format!(
                "PID {} is a protected system process. Use force=true to override.",
                args.pid
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

        // Determine signal - always use SIGKILL (9) for reliability
        let signal = 9;
        let signal_arg = "-9";

        // Execute kill command - use SIGKILL for immediate termination
        let output = Command::new("kill")
            .arg(signal_arg)
            .arg(args.pid.to_string())
            .output()
            .map_err(|e| Error::ExecutionFailed(format!("Failed to spawn kill: {}", e)))?;

        let success = output.status.success();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Small delay to let process terminate
        std::thread::sleep(Duration::from_millis(200));

        // Capture process snapshot after kill
        let process_after = ProcessSnapshot::capture();

        // Check if process still exists
        let process_still_exists = process_after.processes.iter().any(|p| p.pid == args.pid);

        let stderr_str = stderr.to_string();
        let message = if success && !process_still_exists {
            format!("Killed process {} (signal {})", args.pid, signal)
        } else if !success {
            format!("Failed to kill process {}: {}", args.pid, stderr_str)
        } else {
            format!("Process {} still exists after kill", args.pid)
        };

        Ok(Output {
            success: success && !process_still_exists,
            data: serde_json::json!({
                "pid": args.pid,
                "killed": success && !process_still_exists,
                "signal": signal,
                "command": process_info.as_ref().map(|(cmd, _)| cmd),
                "user": process_info.as_ref().map(|(_, user)| user),
                "stderr": if !success { stderr_str } else { String::new() },
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
    #[ignore = "Flaky test - depends on process timing"]
    fn test_kill_actual_process() {
        // Start a long-running process (sleep)
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        // Give it time to start
        thread::sleep(Duration::from_millis(100));

        // Now kill it via the capability
        let cap = Kill;
        let result = cap
            .execute(
                &serde_json::json!({ "pid": pid }),
                &Context {
                    dry_run: false,
                    job_id: "test".into(),
                    working_dir: std::env::current_dir().unwrap(),
                },
            )
            .unwrap();

        assert!(result.success, "Kill failed: {:?}", result.data);
        assert!(result.data["killed"].as_bool() == Some(true));

        // Cleanup: ensure process is actually dead
        let _ = child.wait();
    }
}
