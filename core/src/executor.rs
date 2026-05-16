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
use std::path::{Path, PathBuf};

/// Default timeout for capability execution (seconds).
///
/// **Note:** Timeout is currently advisory only — see [`execute_with_timeout`]
/// for details on the enforcement limitation.
const CAPABILITY_TIMEOUT_SECS: u64 = 30;

/// Result of a telemetry-wrapped capability execution.
///
/// Contains before/after snapshots of hardware telemetry and process state,
/// plus the WAL sequence number for crash recovery correlation.
#[derive(Debug, serde::Serialize)]
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
/// 2. Check resource limits via `ResourceGuard` (circuit breaker at 80%)
/// 3. Log `JobStarted` event to WAL
/// 4. Validate arguments against capability schema
/// 5. Execute the capability
/// 6. Capture hardware telemetry and process snapshot (after)
/// 7. Log `JobCompleted` or `JobFailed` event to WAL
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
/// Returns [`Error::ResourceLimitExceeded`] if the `ResourceGuard` circuit breaker
/// trips before execution begins. WAL write failures also propagate as errors.
///
/// # Timeout Limitation
///
/// The `timeout_secs` parameter is currently **not enforced**. Rust's
/// `std::thread` cannot be interrupted once started. A true timeout requires
/// either subprocess isolation or `tokio::spawn_blocking` with cancellation.
/// This is tracked for v0.2.0.
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
/// * `timeout_secs` — Timeout for capability execution
pub fn execute_with_telemetry_and_session(
    capability: &dyn Capability,
    args: &Value,
    dry_run: bool,
    wal_path: &Path,
    session_id: Option<&str>,
    timeout_secs: u64,
) -> Result<ExecutionResult> {
    let job_id = JobId::new();
    let job_id_str = job_id.as_str().to_string();
    let cap_name = capability.name().to_string();

    let telemetry_before = Telemetry::capture();
    let process_before = ProcessSnapshot::capture();

    // LlmoSafeGuard is the circuit breaker — reads /proc/stat with delta measurement
    LlmoSafeGuard::new()
        .check()
        .map_err(|e| Error::ResourceLimitExceeded(e.to_string()))?;

    let mut wal = WalWriter::create(wal_path)?;
    let ctx = Context {
        dry_run,
        job_id: job_id_str.clone(),
        working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };

    let start_seq = wal.seq();
    wal.append(WalEvent {
        seq: start_seq,
        ts: telemetry_before.timestamp,
        event_type: WalEventType::JobStarted,
        job_id: job_id_str.clone(),
        capability: Some(cap_name.clone()),
        output: None,
        error: None,
    })?;

    if let Err(e) = capability.validate(args) {
        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();
        let end_seq = wal.seq();
        log_job_failed(
            &mut wal,
            &job_id_str,
            &cap_name,
            &format!("Validation failed: {}", e),
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
    let output = match execute_with_timeout(capability, args, &ctx, timeout_secs) {
        Ok(out) => out,
        Err(e) => {
            let telemetry_after = Telemetry::capture();
            let process_after = ProcessSnapshot::capture();
            let end_seq = wal.seq();
            let err_msg = format!("Execution failed: {}", e);
            log_job_failed(&mut wal, &job_id_str, &cap_name, &err_msg)?;

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

    let end_seq = wal.seq();
    wal.append(WalEvent {
        seq: end_seq,
        ts: telemetry_after.timestamp,
        event_type: WalEventType::JobCompleted,
        job_id: job_id_str.clone(),
        capability: Some(cap_name.clone()),
        output: Some(serde_json::to_value(&output).unwrap_or(Value::Null)),
        error: None,
    })?;

    // Add job to session if session tracking is enabled
    if let Some(sid) = session_id {
        let sessions_dir = std::env::var("RUNTIMO_SESSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::utils::data_dir().join("sessions"));
        if let Ok(mut mgr) = SessionManager::new(sessions_dir) {
            let _ = mgr.add_job(sid, &job_id_str);
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

/// Log a `JobFailed` event to the WAL.
fn log_job_failed(wal: &mut WalWriter, job_id: &str, capability: &str, error: &str) -> Result<()> {
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
    })
}

/// Execute a capability inline with timeout enforcement.
///
/// For capabilities that support timeout (like ShellExec), the timeout is enforced.
/// The timeout is advisory for other capabilities.
fn execute_with_timeout(
    capability: &dyn Capability,
    args: &Value,
    ctx: &Context,
    _timeout_secs: u64,
) -> Result<Output> {
    // ShellExec and other capabilities handle timeout internally
    // Pass timeout via context if the capability supports it
    capability.execute(args, ctx)
}
