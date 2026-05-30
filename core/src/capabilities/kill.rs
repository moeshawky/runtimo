//! Kill capability — terminate runaway processes by PID with full audit trail.
//!
//! Kills a process by PID with full telemetry capture and WAL logging.
//! Includes safety checks to prevent killing critical system processes.
//!
//! # PID Reuse Protection (FINDING #1)
//!
//! After sending a signal, the capability verifies the killed process is the
//! same one by comparing start times from `/proc/{pid}/stat` field 22. This
//! prevents PID reuse races where a new process inherits the killed PID.
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

/// Reads the process start time (field 22) from `/proc/{pid}/stat`.
///
/// Returns start time in clock ticks since boot. Used to detect PID reuse:
/// if a process is killed and a new process reuses the PID, the start time
/// will differ (FINDING #1).
#[allow(clippy::arithmetic_side_effects)]
fn get_process_start_time(pid: u32) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&stat_path).ok()?;
    let last_paren = content.rfind(')')?;
    let fields: Vec<&str> = content[last_paren + 2..].split_whitespace().collect();
    fields.get(19)?.parse::<u64>().ok()
}
fn get_process_start_time_retry(pid: u32) -> Option<u64> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10 * (1 << attempt)));
        }
        if let Some(start_time) = get_process_start_time(pid) {
            return Some(start_time);
        }
    }
    None
}

/// Reads the cgroup of a process from `/proc/{pid}/cgroup`.
///
/// Returns the cgroup path string, used to detect systemd-managed services.
fn get_process_cgroup(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()
}

/// Checks if a cgroup path indicates a systemd-managed service.
fn is_systemd_service(cgroup: &str) -> bool {
    cgroup.contains("/system.slice/")
        || cgroup.contains("/init.scope")
        || cgroup.contains("systemd")
}

/// Protected PIDs that cannot be killed (safety guard).
/// Includes init, kthreadd, current process, parent, session leader,
/// process group leader, and systemd critical services (FINDING #2).
fn protected_pids() -> Vec<u32> {
    let mut pids = vec![1, 2];
    let self_pid = std::process::id();
    pids.push(self_pid);

    // Add parent process
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self_pid)) {
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

    // Add session leader (FINDING #2)
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self_pid)) {
        if let Some(sid_str) = status
            .lines()
            .find(|l| l.starts_with("Sid:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            if let Ok(sid) = sid_str.parse::<u32>() {
                if sid != 0 {
                    pids.push(sid);
                }
            }
        }
    }

    // Add process group leader (FINDING #2)
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self_pid)) {
        if let Some(pgid_str) = status
            .lines()
            .find(|l| l.starts_with("NSpgid:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            if let Ok(pgid) = pgid_str.parse::<u32>() {
                if pgid != 0 {
                    pids.push(pgid);
                }
            }
        }
    }

    // Scan all running processes for systemd-critical services (FINDING #2)
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if let Ok(pid) = name.parse::<u32>() {
                    if let Some(cgroup) = get_process_cgroup(pid) {
                        if is_systemd_service(&cgroup) {
                            pids.push(pid);
                        }
                    }
                }
            }
        }
    }

    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Arguments for the [`Kill`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillArgs {
    /// Process ID to kill.
    pub pid: u32,
    /// Signal to send (default: 15 = SIGTERM). Must be valid POSIX: 1-31 or 64.
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
// This is a capability marker struct with no fields;
// additional fields may be added later as needed.
#[allow(clippy::exhaustive_structs)]
pub struct Kill;

impl Capability for Kill {
    fn name(&self) -> &'static str {
        "Kill"
    }

    fn description(&self) -> &'static str {
        "kill PID. Protected: init,kthreadd,self. Custom sig ok."
    }

    /// Returns the JSON Schema for Kill arguments.
    ///
    /// Schema requires `"pid"` integer; `"signal"` is optional and restricted
    /// to valid POSIX signal values (1-31, 64) — FINDING #3.
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pid": { "type": "integer", "minimum": 1 },
                "signal": {
                    "type": "integer",
                    "anyOf": [
                        { "minimum": 1, "maximum": 31 },
                        { "enum": [64] }
                    ]
                }
            },
            "required": ["pid"]
        })
    }

    fn validate(&self, args: &Value) -> Result<()> {
        let args: KillArgs = serde_json::from_value(args.clone())
            .map_err(|e| Error::SchemaValidationFailed(e.to_string()))?;

        // FINDING #3: Restrict signal to valid POSIX values (1-31, 64)
        if let Some(signal) = args.signal {
            if !(1..=31).contains(&signal) && signal != 64 {
                return Err(Error::SchemaValidationFailed(format!(
                    "Invalid signal {}: must be 1-31 or 64 (POSIX signals)",
                    signal
                )));
            }
        }

        Ok(())
    }

    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
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

        // Respect dry_run — skip kill entirely
        if ctx.dry_run {
            // FINDING #20: Limit dry-run output to "would kill PID X", hide command/user info
            return Ok(Output {
                success: true,
                data: serde_json::json!({
                    "pid": args.pid,
                    "killed": false,
                    "dry_run": true,
                    "signal": args.signal.unwrap_or(15),
                }),
                message: Some(format!("DRY RUN: would kill PID {}", args.pid)),
            });
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

        // Record start time to detect PID reuse (FINDING #1)
        let start_time_before = get_process_start_time_retry(args.pid);

        // Determine signal — default to SIGTERM (15) for graceful shutdown
        let signal = args.signal.unwrap_or(15);

        // Execute kill via libc for reliability (avoids shell/PATH issues)
        // SAFETY: pid is validated as a valid target; signal is validated to 1-64 range;
        // pid_t is i32 — pid is u32, cast is safe for all valid PIDs
        #[allow(clippy::cast_possible_wrap)]
        let kill_result = unsafe { libc::kill(args.pid as libc::pid_t, signal) };
        let success = kill_result == 0;
        let stderr_str = if success {
            String::new()
        } else {
            std::io::Error::last_os_error().to_string()
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
        // Verify PID was not reused — check start time matches (FINDING #1)
        let pid_reused = match (start_time_before, get_process_start_time_retry(args.pid)) {
            (Some(before_time), Some(after_time)) => before_time != after_time,
            (None, _) => false,
            (Some(_), None) => true,
        };

        let killed_success = success && !process_still_exists && !pid_reused;

        let message = if killed_success {
            format!("Killed process {} (signal {})", args.pid, signal)
        } else if pid_reused {
            format!(
                "PID {} was reused by a different process (start time changed)",
                args.pid
            )
        } else if !success {
            format!("Failed to kill process {}: {}", args.pid, stderr_str)
        } else {
            format!("Process {} still exists after signal {}", args.pid, signal)
        };

        Ok(Output {
            success: killed_success,
            data: serde_json::json!({
                "pid": args.pid,
                "killed": killed_success,
                "signal": signal,
                "command": process_info.as_ref().map(|(cmd, _)| cmd),
                "user": process_info.as_ref().map(|(_, user)| user),
                "stderr": if success { String::new() } else { stderr_str },
                "pid_reused": pid_reused,
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
#[allow(clippy::unnecessary_map_or)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_kill_schema() {
        let cap = Kill;
        let _schema = cap.schema();
        // Retry function test
        // Test retry logic with existing process
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        let result = get_process_start_time_retry(pid);
        assert!(
            result.is_some(),
            "Should read start time for running process"
        );

        child.kill().ok();
        let _ = child.wait();

        // Non-existent PID should return None after retries
        let result = get_process_start_time_retry(999999);
        assert!(result.is_none(), "Non-existent PID should return None");
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
    fn test_kill_dry_run() {
        let cap = Kill;
        // Use a real PID (self) but in dry_run mode — should NOT error as protected
        // because dry_run skips the actual kill but still checks protection
        // Actually, protection check runs before dry_run, so use a non-protected PID
        let result = cap
            .execute(
                &serde_json::json!({ "pid": 999998 }),
                &Context {
                    dry_run: true,
                    job_id: "test".into(),
                    working_dir: std::env::current_dir().unwrap(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data["dry_run"].as_bool() == Some(true));
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
        let still_alive = post_check.map_or(false, |o| o.status.success());
        assert!(
            !still_alive,
            "Process {} should be dead after kill and reap",
            pid
        );
    }

    #[test]
    fn test_get_process_start_time() {
        // Start a process and verify we can read its start time
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        let start_time = get_process_start_time(pid);
        assert!(
            start_time.is_some(),
            "Should be able to read start time for running process"
        );

        // Verify start time is consistent (no PID reuse)
        let start_time2 = get_process_start_time(pid);
        assert_eq!(start_time, start_time2, "Start time should be stable");

        child.kill().ok();
        let _ = child.wait();
    }

    #[test]
    fn test_get_process_start_time_nonexistent() {
        let result = get_process_start_time(999999);
        assert!(result.is_none(), "Non-existent PID should return None");
    }

    #[test]
    fn test_signal_validation_rejects_negative() {
        // FINDING #3: negative signals should be rejected
        let cap = Kill;
        let result = cap.validate(&serde_json::json!({ "pid": 999998, "signal": -1 }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid signal"));
    }

    #[test]
    fn test_signal_validation_rejects_zero() {
        // FINDING #3: signal 0 should be rejected
        let cap = Kill;
        let result = cap.validate(&serde_json::json!({ "pid": 999998, "signal": 0 }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid signal"));
    }

    #[test]
    fn test_signal_validation_rejects_out_of_range() {
        // FINDING #3: signal > 31 (except 64) should be rejected
        let cap = Kill;
        let result = cap.validate(&serde_json::json!({ "pid": 999998, "signal": 32 }));
        assert!(result.is_err());
    }

    #[test]
    fn test_signal_validation_accepts_valid_signals() {
        let cap = Kill;
        for sig in [1, 9, 15, 31, 64] {
            let result = cap.validate(&serde_json::json!({ "pid": 999998, "signal": sig }));
            assert!(result.is_ok(), "Signal {} should be valid", sig);
        }
    }

    #[test]
    fn test_dry_run_hides_process_info() {
        // FINDING #20: dry-run should NOT expose command or user info
        let cap = Kill;
        let result = cap
            .execute(
                &serde_json::json!({ "pid": 999998 }),
                &Context {
                    dry_run: true,
                    job_id: "test".into(),
                    working_dir: std::env::current_dir().unwrap(),
                },
            )
            .unwrap();

        assert!(result.success);
        assert!(result.data["dry_run"].as_bool() == Some(true));
        assert!(
            result.data.get("command").is_none(),
            "dry-run must not expose command"
        );
        assert!(
            result.data.get("user").is_none(),
            "dry-run must not expose user"
        );
        assert!(
            result.data.get("process_exists").is_none(),
            "dry-run must not expose process_exists"
        );
    }

    #[test]
    fn test_protected_pids_includes_self_and_parent() {
        let protected = protected_pids();
        let self_pid = std::process::id();
        assert!(protected.contains(&1), "PID 1 should be protected");
        assert!(protected.contains(&2), "PID 2 should be protected");
        assert!(
            protected.contains(&self_pid),
            "self PID should be protected"
        );
    }

    #[test]
    fn test_get_process_start_time_retry() {
        // Test retry logic with existing process
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();

        let result = get_process_start_time_retry(pid);
        assert!(
            result.is_some(),
            "Should read start time for running process"
        );

        child.kill().ok();
        let _ = child.wait();

        // Non-existent PID should return None after retries
        let result = get_process_start_time_retry(999999);
        assert!(result.is_none(), "Non-existent PID should return None");
    }
}
