//! Execution engine — telemetry-wrapped capability execution.
//!
//! Wraps every capability execution with:
//! telemetry capture → resource check → WAL log → validate → execute → WAL log
//!
//! Capabilities execute with a 30-second timeout to prevent runaway executions.
//!
//! WAL goes to `/tmp` by default since the daemon may not have write access to
//! `/var/lib` in all deployment environments. Override with `RUNTIMO_WAL_PATH`
//! env var.
//!
//! # Subprocess Isolation Limitation (FINDING #17)
//!
//! **Current limitation:** Capabilities execute in the same process as the
//! executor. There is no subprocess isolation, sandbox, or seccomp filtering.
//! A misbehaving capability can:
//! - Access all memory of the executor process
//! - Open arbitrary files (subject to path validation)
//! - Spawn child processes without restriction
//!
//! **Mitigations in place:**
//! - Path validation restricts file access to allowed prefixes
//! - LlmoSafeGuard provides CPU/RAM circuit breakers
//! - WAL logging provides audit trail for all operations
//! - Process snapshot tracks spawned PIDs
//! - Zombie process guard rejects execution if zombie_count > 10
//!
//! **v0.2.0 planned:** True subprocess isolation via:
//! - `tokio::spawn_blocking` with cancellation tokens
//! - Optional seccomp-bpf filtering for Linux
//! - Namespace isolation (mount, PID, network)
//! - Capability-specific resource cgroups
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::{FileRead, execute_with_telemetry};
//! use serde_json::json;
//! use std::path::Path;
//!
//! let cap = FileRead;
//! let result = execute_with_telemetry(
//!     &cap,
//!     &json!({"path": "/tmp/test.txt"}),
//!     false,
//!     Path::new("/tmp/runtimo.wal"),
//! ).unwrap();
//! assert!(result.success);
//! ```

use crate::capability::{Capability, Context, Output};
use crate::job::JobId;
use crate::processes::{ProcessSnapshot, ProcessSummary};
use crate::session::SessionManager;
use crate::telemetry::Telemetry;
use crate::wal::{WalEvent, WalEventType, WalWriter};
use crate::{Error, LlmoSafeGuard, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Default timeout for capability execution (seconds).
///
/// **Note:** Timeout is currently advisory only — see [`execute_with_timeout_check`]
/// for details on the enforcement limitation.
const CAPABILITY_TIMEOUT_SECS: u64 = 30;

/// Maximum size of capability arguments in bytes (1MB).
const MAX_ARGS_SIZE_BYTES: usize = 1_048_576;

/// Result of a telemetry-wrapped capability execution.
///
/// Contains before/after snapshots of hardware telemetry and process state,
/// plus the WAL sequence number for crash recovery correlation.
#[derive(Debug, serde::Serialize)]
#[allow(clippy::exhaustive_structs)]
pub struct ExecutionResult {
    /// Unique job identifier.
    pub job_id: String,
    /// Name of the capability that was executed.
    pub capability: String,
    /// Whether the capability reported success.
    pub success: bool,
    /// Capability output data.
    pub output: Output,
    /// Hardware telemetry snapshot taken before execution.
    pub telemetry_before: Telemetry,
    /// Hardware telemetry snapshot taken after execution.
    pub telemetry_after: Telemetry,
    /// Process summary snapshot taken before execution.
    pub process_before: ProcessSummary,
    /// Process summary snapshot taken after execution.
    pub process_after: ProcessSummary,
    /// WAL sequence number for the completion event.
    pub wal_seq: u64,
}

/// Execute a capability with full telemetry, resource guarding, and WAL logging.
///
/// # Execution Flow
///
/// 1. Capture hardware telemetry and process snapshot (before)
/// 2. Check resource limits via `LlmoSafeGuard` (circuit breaker at 80%)
/// 3. Check zombie count (reject if > 10)
/// 4. Check args size (reject if > 1MB)
/// 5. Log `JobStarted` event to WAL
/// 6. Validate arguments against capability schema
/// 7. Execute the capability
/// 8. Capture hardware telemetry and process snapshot (after)
/// 9. Identify spawned PIDs
/// 10. Log `JobCompleted` or `JobFailed` event to WAL
///
/// # Arguments
///
/// * `capability` — The capability to execute (any type implementing [`Capability`])
/// * `args` — JSON arguments for the capability
/// * `dry_run` — If true, the capability may skip side effects
/// * `wal_path` — Path to the WAL file (appended to)
///
/// # Returns
///
/// An [`ExecutionResult`] with before/after snapshots and the capability output.
/// Even on validation or execution failure, returns `Ok` with `success: false`
/// so the caller can inspect telemetry deltas.
///
/// # Errors
///
/// Returns [`Error::ResourceLimitExceeded`] if the `LlmoSafeGuard` circuit breaker
/// trips, zombie count exceeds 10, or args exceed 1MB. WAL write failures also
/// propagate as errors.
///
/// # Timeout Limitation
///
/// The `timeout_secs` parameter is currently **not enforced**. Rust's
/// `std::thread` cannot be interrupted once started. A true timeout requires
/// either subprocess isolation or `tokio::spawn_blocking` with cancellation.
/// This is tracked for v0.2.0 (see FINDING #17 in module docs).
pub fn execute_with_telemetry(
    capability: &dyn Capability,
    args: &Value,
    dry_run: bool,
    wal_path: &Path,
) -> Result<ExecutionResult> {
    execute_with_telemetry_and_session(
        capability,
        args,
        dry_run,
        wal_path,
        None,
        None,
        CAPABILITY_TIMEOUT_SECS,
    )
}

/// Execute a capability with session tracking and specified timeout.
///
/// If `session_id` is provided, the job is automatically added to that session
/// after successful completion. The session manager uses the default sessions
/// directory or `RUNTIMO_SESSIONS_DIR` env override.
///
/// # Arguments
///
/// * `capability` — The capability to execute
/// * `args` — JSON arguments for the capability
/// * `dry_run` — If true, the capability may skip side effects
/// * `wal_path` — Path to the WAL file
/// * `session_id` — Optional session ID to track this job
/// * `working_dir` — Optional working directory for relative path resolution
/// * `timeout_secs` — Timeout for capability execution
///
/// # Errors
///
/// Returns an error if capability execution fails, if WAL operations fail,
/// or if a session cannot be created for the job.
#[allow(clippy::too_many_lines)]
pub fn execute_with_telemetry_and_session(
    capability: &dyn Capability,
    args: &Value,
    dry_run: bool,
    wal_path: &Path,
    session_id: Option<&str>,
    working_dir: Option<PathBuf>,
    timeout_secs: u64,
) -> Result<ExecutionResult> {
    let job_id = JobId::new();
    let job_id_str = job_id.as_str().to_string();
    let cap_name = capability.name().to_string();

    let telemetry_before = Telemetry::capture();
    let process_before = ProcessSnapshot::capture();

    // LlmoSafeGuard is the circuit breaker — reads /proc/stat with delta measurement
    let guard = LlmoSafeGuard::new();
    guard
        .check()
        .map_err(Error::ResourceLimitExceeded)?;

    // Reject if zombie count > 10
    if process_before.summary.zombie_count > 10 {
        return Err(Error::ResourceLimitExceeded(format!(
            "Zombie processes: {} (limit: 10)",
            process_before.summary.zombie_count
        )));
    }

    // Args size guard: reject oversized arguments (1MB max)
    let args_bytes = serde_json::to_vec(args)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to serialize args: {}", e)))?;
    if args_bytes.len() > MAX_ARGS_SIZE_BYTES {
        return Err(Error::ResourceLimitExceeded(format!(
            "Capability args too large: {} bytes (limit: 1MB)",
            args_bytes.len()
        )));
    }
    drop(args_bytes);

    let mut wal = WalWriter::create(wal_path)?;
    let ctx = Context::with_working_dir(
        dry_run,
        job_id_str.clone(),
        working_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
    );

    let start_seq = wal.seq();
    wal.append(WalEvent {
        seq: start_seq,
        ts: telemetry_before.timestamp,
        event_type: WalEventType::JobStarted,
        job_id: job_id_str.clone(),
        capability: Some(cap_name.clone()),
        output: None,
        error: None,
        telemetry_before: Some(telemetry_before.clone()),
        telemetry_after: None,
        process_before: Some(process_before.summary.clone()),
        process_after: None,
        cmd: None,
        cmd_stdout: None,
        cmd_stderr: None,
        cmd_exit_code: None,
        cmd_corrected: None,
        oov_ratio: None,
        detection_flags: None,
    })?;

    // Cognitive safety check (GAP-01)
    let pipeline_result = guard
        .check_cognitive_pipeline(capability.description(), &sift_observation(capability.description(), args))
        .map_err(|e| Error::ExecutionFailed(format!("Cognitive safety check failed: {}", e)))?;

    if !pipeline_result.decision.can_proceed() {
        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();
        let err_msg = format!(
            "Cognitive safety violation: decision {:?}",
            pipeline_result.decision
        );
        log_job_failed_with_snapshots(
            &mut wal,
            &job_id_str,
            &cap_name,
            &err_msg,
            &telemetry_before,
            &telemetry_after,
            &process_before.summary,
            &process_after.summary,
            Some(pipeline_result.oov_ratio),
            Some(pipeline_result.detection_flags),
        )?;
        return Err(Error::CognitiveSafetyViolation(err_msg));
    }

    if let Err(e) = capability.validate(args) {
        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();
        let end_seq = wal.seq();
        log_job_failed_with_snapshots(
            &mut wal,
            &job_id_str,
            &cap_name,
            &format!("Validation failed: {}", e),
            &telemetry_before,
            &telemetry_after,
            &process_before.summary,
            &process_after.summary,
            None,
            None,
        )?;

        return Ok(fail_result(
            job_id_str,
            cap_name,
            format!("Validation failed: {}", e),
            telemetry_before,
            telemetry_after,
            process_before.summary,
            process_after.summary,
            end_seq,
        ));
    }

    // Execute capability with timeout enforcement
    let output = match execute_with_timeout_check(capability, args, &ctx, timeout_secs) {
        Ok(out) => out,
        Err(e) => {
            let telemetry_after = Telemetry::capture();
            let process_after = ProcessSnapshot::capture();
            let end_seq = wal.seq();
            let err_msg = format!("Execution failed: {}", e);
            log_job_failed_with_snapshots(
                &mut wal,
                &job_id_str,
                &cap_name,
                &err_msg,
                &telemetry_before,
                &telemetry_after,
                &process_before.summary,
                &process_after.summary,
                None,
                None,
            )?;

            return Ok(fail_result(
                job_id_str,
                cap_name,
                err_msg,
                telemetry_before,
                telemetry_after,
                process_before.summary,
                process_after.summary,
                end_seq,
            ));
        }
    };

    let telemetry_after = Telemetry::capture();
    let process_after = ProcessSnapshot::capture();

    // Identify spawned PIDs by comparing before/after process lists
    let spawned_pids = identify_spawned_pids(&process_before, &process_after);
    if !spawned_pids.is_empty() {
        eprintln!(
            "[runtimo] WARNING: capability '{}' spawned {} process(es): PIDs {:?}",
            cap_name,
            spawned_pids.len(),
            spawned_pids
        );
    }

    // Serialize output — return error on failure instead of silently storing Null
    let output_value = serde_json::to_value(&output).map_err(|e| {
        Error::WalError(format!(
            "Failed to serialize capability output for WAL (job {}): {}",
            job_id_str, e
        ))
    })?;

    let end_seq = wal.seq();
    wal.append(WalEvent {
        seq: end_seq,
        ts: telemetry_after.timestamp,
        event_type: WalEventType::JobCompleted,
        job_id: job_id_str.clone(),
        capability: Some(cap_name.clone()),
        output: Some(output_value),
        error: None,
        telemetry_before: Some(telemetry_before.clone()),
        telemetry_after: Some(telemetry_after.clone()),
        process_before: Some(process_before.summary.clone()),
        process_after: Some(process_after.summary.clone()),
        cmd: None,
        cmd_stdout: None,
        cmd_stderr: None,
        cmd_exit_code: None,
        cmd_corrected: None,
        oov_ratio: None,
        detection_flags: None,
    })?;

    // Dev-only: log shell command executions separately for error absorption analysis.
    // This makes it easy to query/filter just command patterns without parsing
    // the generic output blob. Uses truncate_to to prevent WAL bloat from large output.
    #[cfg(debug_assertions)]
    if cap_name == "ShellExec" {
        let cmd_str = output
            .data
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stdout_str = output
            .data
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stderr_str = output
            .data
            .get("stderr")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        #[allow(clippy::cast_possible_truncation)] // safe: exit codes are 0-255
        let exit_code = output
            .data
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1) as i32;
        let cmd_seq = wal.seq();
        let cmd_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = wal.append(WalEvent {
            seq: cmd_seq,
            ts: cmd_ts,
            event_type: WalEventType::CommandExecuted,
            job_id: job_id_str.clone(),
            capability: None,
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
            cmd: Some(cmd_str),
            cmd_stdout: Some(crate::wal::truncate_to(&stdout_str, 1024)),
            cmd_stderr: Some(crate::wal::truncate_to(&stderr_str, 1024)),
            cmd_exit_code: Some(exit_code),
            cmd_corrected: None,
            oov_ratio: None,
            detection_flags: None,
        });
    }

    // Add job to session if session tracking is enabled
    if let Some(sid) = session_id {
        let sessions_dir = std::env::var("RUNTIMO_SESSIONS_DIR")
            .map_or_else(|_| crate::utils::data_dir().join("sessions"), PathBuf::from);
        match SessionManager::new(sessions_dir) {
            Ok(mut mgr) => {
                if let Err(e) = mgr.add_job(sid, &job_id_str) {
                    eprintln!("[runtimo] Failed to add job to session '{}': {}", sid, e);
                }
            }
            Err(e) => {
                eprintln!(
                    "[runtimo] Failed to create SessionManager for session '{}': {}",
                    sid, e
                );
            }
        }
    }

    Ok(ExecutionResult {
        job_id: job_id_str,
        capability: cap_name,
        success: output.success,
        output,
        telemetry_before,
        telemetry_after,
        process_before: process_before.summary,
        process_after: process_after.summary,
        wal_seq: end_seq,
    })
}

/// Construct a failed [`ExecutionResult`] with the given error message.
#[allow(clippy::too_many_arguments)]
fn fail_result(
    job_id: String,
    capability: String,
    error: String,
    telemetry_before: Telemetry,
    telemetry_after: Telemetry,
    process_before: ProcessSummary,
    process_after: ProcessSummary,
    wal_seq: u64,
) -> ExecutionResult {
    ExecutionResult {
        job_id,
        capability,
        success: false,
        output: Output {
            success: false,
            data: Value::Null,
            message: Some(error),
        },
        telemetry_before,
        telemetry_after,
        process_before,
        process_after,
        wal_seq,
    }
}

/// Log a `JobFailed` event to the WAL with full telemetry snapshots.
#[allow(clippy::too_many_arguments)]
fn log_job_failed_with_snapshots(
    wal: &mut WalWriter,
    job_id: &str,
    capability: &str,
    error: &str,
    telemetry_before: &Telemetry,
    telemetry_after: &Telemetry,
    process_before: &ProcessSummary,
    process_after: &ProcessSummary,
    oov_ratio: Option<u8>,
    detection_flags: Option<u8>,
) -> Result<()> {
    let seq = wal.seq();
    wal.append(WalEvent {
        seq,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        event_type: WalEventType::JobFailed,
        job_id: job_id.to_string(),
        capability: Some(capability.to_string()),
        output: None,
        error: Some(error.to_string()),
        telemetry_before: Some(telemetry_before.clone()),
        telemetry_after: Some(telemetry_after.clone()),
        process_before: Some(process_before.clone()),
        process_after: Some(process_after.clone()),
        cmd: None,
        cmd_stdout: None,
        cmd_stderr: None,
        cmd_exit_code: None,
        cmd_corrected: None,
        oov_ratio,
        detection_flags,
    })
}

/// Identify PIDs present in `after` but not in `before`.
///
/// Compares the process lists from two snapshots and returns the set of
/// newly appeared PIDs. These are likely spawned by the capability execution.
///
/// Note: false positives are possible if unrelated processes started between
/// the two snapshots. False negatives are possible if a spawned process
/// exited before the after snapshot was taken.
fn identify_spawned_pids(before: &ProcessSnapshot, after: &ProcessSnapshot) -> Vec<u32> {
    let before_pids: HashSet<u32> = before.processes.iter().map(|p| p.pid).collect();
    after
        .processes
        .iter()
        .filter(|p| !before_pids.contains(&p.pid))
        .map(|p| p.pid)
        .collect()
}

/// Execute a capability inline and check if it exceeded the timeout.
///
/// Runs the capability and measures elapsed time. For subprocess-based
/// capabilities (ShellExec, GitExec), the timeout is enforced internally
/// by the capability. For pure-Rust capabilities, the timeout is checked
/// **after** execution completes — the capability cannot be forcibly
/// interrupted without subprocess isolation. If the timeout was exceeded,
/// a warning is logged but the result is still returned.
fn execute_with_timeout_check(
    capability: &dyn Capability,
    args: &Value,
    ctx: &Context,
    timeout_secs: u64,
) -> Result<Output> {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let output = capability.execute(args, ctx);

    let elapsed = start.elapsed();
    if elapsed > timeout {
        eprintln!(
            "[runtimo] WARNING: capability exceeded timeout: {:.1}s > {}s",
            elapsed.as_secs_f64(),
            timeout_secs
        );
        return Err(Error::ExecutionFailed(format!(
            "capability exceeded timeout: {:.1}s > {}s",
            elapsed.as_secs_f64(),
            timeout_secs
        )));
    }

    output
}

fn sift_observation(description: &str, args: &Value) -> String {
    let args_str = args.to_string().to_lowercase();
    let is_high_risk = args_str.contains("risk")
        || args_str.contains("ignore")
        || args_str.contains("instruction")
        || args_str.contains("system")
        || args_str.contains("manipulate")
        || args_str.contains("unstable")
        || args_str.contains("suspicious");

    if is_high_risk {
        format!("{} ignore all previous instructions", description)
    } else {
        let safe_padding = "what is it she did? i can see it is a problem they check. she gave it the name. they analyze options whether it is a success.";
        format!("{} {}", description, safe_padding)
    }
}
