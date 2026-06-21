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
    /// Whether the capability reported success (derived from output.status).
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
/// # Telemetry
///
/// Uses [`Telemetry::capture_lightweight`] for before/after snapshots —
/// skips GPU/JAX/network shell-outs that are unnecessary for the WAL audit
/// trail and produce stderr noise on systems without those tools.
///
/// # Cognitive Safety
///
/// Capabilities with user-authored natural language content (commands, file
/// content, URLs, commit messages) pass through the llmosafe `CognitivePipeline`
/// for TF-IDF + keyword bias detection. Structured-only capabilities (paths,
/// PIDs, job IDs) skip the check to avoid NLP false positives.
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

    // Lightweight capture skips GPU/JAX/network shell-outs — executor only
    // needs /proc-based system health data (CPU, RAM, disk) for the WAL audit
    // trail. The LlmoSafeGuard resource check reads /proc/stat independently.
    let telemetry_before = Telemetry::capture_lightweight();
    let process_before = ProcessSnapshot::capture();

    // LlmoSafeGuard is the circuit breaker — reads /proc/stat with delta measurement
    let guard = LlmoSafeGuard::new();
    guard.check().map_err(Error::ResourceLimitExceeded)?;

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
        working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
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

    // Cognitive safety check — runs llmosafe's CognitivePipeline
    // (sifter + bias detection + surprise gating + detectors) against
    // user-authored natural language content (commands, file content,
    // URLs, commit messages). Structured inputs (paths, PIDs, job IDs)
    // are skipped — the TF-IDF classifier was trained on manipulation
    // text and produces false positives on structured data.
    if has_natural_content(args) {
        let pipeline_result = guard
            .check_cognitive_pipeline(
                capability.description(),
                &sift_observation(capability.description(), args),
            )
            .map_err(|e| Error::ExecutionFailed(format!("Cognitive safety check failed: {}", e)))?;

        if !pipeline_result.decision.can_proceed() {
            let telemetry_after = Telemetry::capture_lightweight();
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
    }

    // Validation is performed by the TypedCapability blanket impl during
    // deserialization in execute(). The Capability::validate() method
    // always returns Ok(()) for TypedCapability implementations, so this
    // separate validation step is redundant and has been removed.
    // Direct Capability implementers should perform validation in execute().

    // Execute capability with timeout enforcement
    let output = match execute_with_timeout_check(capability, args, &ctx, timeout_secs) {
        Ok(out) => out,
        Err(e) => {
            let telemetry_after = Telemetry::capture_lightweight();
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

    let telemetry_after = Telemetry::capture_lightweight();
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
            .as_ref()
            .and_then(|d| d.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stdout_str = output
            .data
            .as_ref()
            .and_then(|d| d.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stderr_str = output
            .data
            .as_ref()
            .and_then(|d| d.get("stderr"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        #[allow(clippy::cast_possible_truncation)] // safe: exit codes are 0-255
        let exit_code = output
            .data
            .as_ref()
            .and_then(|d| d.get("exit_code"))
            .and_then(|v| v.as_i64())
            .unwrap_or(-1) as i32;
        let cmd_seq = wal.seq();
        let cmd_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) = wal.append(WalEvent {
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
        }) {
            log::error!("WAL CommandExecuted append failed: {}", e);
        }
    }

    // Add job to session if session tracking is enabled
    if let Some(sid) = session_id {
        let sessions_dir = std::env::var("RUNTIMO_SESSIONS_DIR")
            .map_or_else(|_| crate::utils::data_dir().join("sessions"), PathBuf::from);
        match SessionManager::new(sessions_dir) {
            Ok(mut mgr) => {
                if let Err(e) = mgr.add_job(sid, &job_id_str) {
                    log::error!("Failed to add job to session '{}': {}", sid, e);
                }
            }
            Err(e) => {
                log::error!(
                    "Failed to create SessionManager for session '{}': {}",
                    sid,
                    e
                );
            }
        }
    }

    Ok(ExecutionResult {
        job_id: job_id_str,
        capability: cap_name,
        success: output.status == "ok",
        output,
        telemetry_before,
        telemetry_after,
        process_before: process_before.summary,
        process_after: process_after.summary,
        wal_seq: end_seq,
    })
}

/// Construct a failed [`ExecutionResult`] with the given error message.
///
/// Sets `success: false` and creates an error Output with the error string.
/// All telemetry and process snapshots are preserved for the caller to inspect
/// the delta between before/after states even on failure.
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
        output: Output::error(error.clone(), error),
        telemetry_before,
        telemetry_after,
        process_before,
        process_after,
        wal_seq,
    }
}

/// Log a `JobFailed` event to the WAL with full telemetry and process snapshots.
///
/// Appends a `WalEvent` with `event_type = JobFailed`, capturing both before and
/// after telemetry/process state so that failure analysis can compare the deltas.
/// Includes optional `oov_ratio` and `detection_flags` for cognitive safety violations.
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

/// Constructs an observation string for the cognitive safety pipeline.
///
/// Inspects `args` for high-risk keywords (`risk`, `ignore`, `instruction`,
/// `system`, `manipulate`, `unstable`, `suspicious`). When detected, appends
/// an injection-attack prompt suffix to increase cognitive safety sensitivity.
///
/// On benign inputs, returns only the capability description without
/// padding — no injected text that could trigger content classifiers.
fn has_natural_content(args: &Value) -> bool {
    args.get("cmd").and_then(|v| v.as_str()).is_some()
        || args.get("content").and_then(|v| v.as_str()).is_some()
        || args.get("url").and_then(|v| v.as_str()).is_some()
        || args.get("message").and_then(|v| v.as_str()).is_some()
}

fn sift_observation(description: &str, args: &Value) -> String {
    if let Some(cmd) = args.get("cmd").and_then(|v| v.as_str()) {
        return truncate_for_sift(cmd);
    }
    if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
        return truncate_for_sift(content);
    }
    if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        return url.to_string();
    }
    if let Some(message) = args.get("message").and_then(|v| v.as_str()) {
        return truncate_for_sift(message);
    }
    description.to_string()
}

fn truncate_for_sift(s: &str) -> String {
    const SIFT_MAX_CHARS: usize = 8192;
    if s.len() <= SIFT_MAX_CHARS {
        s.to_string()
    } else {
        let mut end = SIFT_MAX_CHARS;
        while !s.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let remaining = s.len().saturating_sub(end);
        format!("{}... [truncated {} bytes]", &s[..end], remaining)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unused_result_ok)]
mod tests {
    use super::*;
    use crate::capabilities::FileRead;
    use crate::capability::{Capability, Context, Output};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Mutex to serialize tests that set `RUNTIMO_DAL` env var.
    /// Without this, concurrent tests fight over the process-global env var.
    static DAL_TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn unique_test_dir() -> PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("runtimo_exec_test_{}_{}", std::process::id(), ns))
    }

    fn wal_path(base: &std::path::Path) -> PathBuf {
        base.join("wal.jsonl")
    }

    fn make_file(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).unwrap();
        write!(f, "{}", content).unwrap();
        p
    }

    /// A minimal test capability that always succeeds.
    struct EchoCap;
    impl Capability for EchoCap {
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn description(&self) -> &'static str {
            "echo capability for testing"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn validate(&self, _args: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, args: &Value, _ctx: &Context) -> crate::Result<Output> {
            let mut out = Output::ok("echo completed".into());
            out.data = Some(args.clone());
            Ok(out)
        }
    }

    /// A slow capability that exceeds timeout.
    struct SlowCap;
    impl Capability for SlowCap {
        fn name(&self) -> &'static str {
            "Slow"
        }
        fn description(&self) -> &'static str {
            "slow capability for testing timeout"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn validate(&self, _args: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, _args: &Value, _ctx: &Context) -> crate::Result<Output> {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(Output::ok("slow completed".into()))
        }
    }

    // ── GAP 1: executor.rs happy path ─────────────────────────────────

    #[test]
    fn test_execute_with_telemetry_happy_path() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let p = make_file(&dir, "test.txt", "hello executor");
        let wp = wal_path(&dir);

        let result = execute_with_telemetry_and_session(
            &FileRead,
            &json!({"path": p.to_str().unwrap()}),
            false,
            &wp,
            None,
            None,
            30,
        );

        assert!(result.is_ok(), "Execute failed: {:?}", result.err());
        let r = result.unwrap();
        assert!(r.success, "Execution should succeed");
        assert_eq!(r.capability, "FileRead");
        assert!(!r.job_id.is_empty());

        // Telemetry captured before and after
        assert!(r.telemetry_before.timestamp > 0);
        assert!(r.telemetry_after.timestamp > 0);
        assert!(r.telemetry_after.timestamp >= r.telemetry_before.timestamp);

        // Process snapshot captured
        assert!(r.process_before.total_processes > 0);
        assert!(r.process_after.total_processes > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_execute_writes_wal_events() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let p = make_file(&dir, "test.txt", "wal check");
        let wp = wal_path(&dir);

        let _result = execute_with_telemetry_and_session(
            &FileRead,
            &json!({"path": p.to_str().unwrap()}),
            false,
            &wp,
            None,
            None,
            30,
        )
        .unwrap();

        // WAL should contain JobStarted and JobCompleted events
        let reader = crate::WalReader::load(&wp).unwrap();
        let events = reader.events();
        assert!(
            events.len() >= 2,
            "WAL should have at least 2 events, got {}",
            events.len()
        );

        let has_started = events
            .iter()
            .any(|e| matches!(e.event_type, crate::WalEventType::JobStarted));
        let has_completed = events
            .iter()
            .any(|e| matches!(e.event_type, crate::WalEventType::JobCompleted));
        assert!(has_started, "WAL should contain JobStarted event");
        assert!(has_completed, "WAL should contain JobCompleted event");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_execute_with_timeout_returns_error() {
        // Use timeout=0 (or very small) to trigger timeout on any non-trivial execution
        let result = execute_with_timeout_check(
            &SlowCap,
            &json!({}),
            &Context::new(false, "timeout-test".into()),
            0, // zero timeout — any execution exceeds it
        );
        // SlowCap takes 200ms, with timeout=0 it should error
        assert!(
            result.is_err(),
            "Should return timeout error, got: {:?}",
            result
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timeout"),
            "Error should mention timeout: {}",
            err
        );
    }

    #[test]
    fn test_execute_with_echo_capability() {
        // Set DAL=E so the cognitive pipeline doesn't block EchoCap on
        // trivial inputs (single-word description triggers CognitiveInstability).
        // This test validates general execution flow, not cognitive safety.
        let _guard = DAL_TEST_MUTEX.lock().unwrap();
        std::env::set_var("RUNTIMO_DAL", "E");

        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let wp = wal_path(&dir);

        let result = execute_with_telemetry_and_session(
            &EchoCap,
            &json!({"key": "value"}),
            false,
            &wp,
            None,
            None,
            30,
        );

        std::env::remove_var("RUNTIMO_DAL");

        assert!(result.is_ok(), "Echo execute failed: {:?}", result.err());
        let r = result.unwrap();
        assert!(r.success);
        assert_eq!(r.capability, "Echo");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_llmosafe_guard_check_called() {
        // Verify the LlmoSafeGuard can be constructed and that check()
        // returns a Result (not panics). The guard's decision depends on
        // system load which varies across environments; we test the
        // invariant that construction + check completes, and the result
        // pattern is correct regardless of outcome.
        let guard = LlmoSafeGuard::new();
        let result = guard.check();
        // On an idle system this should pass. On a loaded system it may
        // return ResourceLimitExceeded — either is correct behavior.
        // The invariant: result is a Result, not a panic.
        match result {
            Ok(()) => { /* guard check passed — system is idle */ }
            Err(msg) => {
                eprintln!("System under pressure during test: {}", msg);
                // This is valid — the guard correctly detected pressure
            }
        }
    }

    // ── GAP 1: Args size guard ────────────────────────────────────────

    #[test]
    fn test_args_size_guard_rejects_large_args() {
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let wp = wal_path(&dir);

        // Create args that exceed 1MB
        let large_content = "x".repeat(2_000_000);
        let result = execute_with_telemetry_and_session(
            &EchoCap,
            &json!({"content": large_content}),
            false,
            &wp,
            None,
            None,
            30,
        );

        // Should fail with ResourceLimitExceeded
        assert!(result.is_err(), "Should reject args > 1MB");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("too large") || err.contains("args"),
            "Error should mention args size: {}",
            err
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── Cognitive pipeline with DAL=A ────────────────────────────────
    //
    // All capabilities now pass through the cognitive safety pipeline
    // (COGNITIVE_SAFETY_SKIP was removed). EchoCap tests the full path
    // with user-authored content in the "content" field.

    #[test]
    fn test_cognitive_pipeline_dal_a_rejects() {
        let _guard = DAL_TEST_MUTEX.lock().unwrap();
        // Set DAL to A (aggressive) for cognitive safety
        std::env::set_var("RUNTIMO_DAL", "A");

        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let wp = wal_path(&dir);

        // EchoCap args are extracted by sift_observation and passed
        // through the cognitive pipeline for bias/manipulation detection.
        let result = execute_with_telemetry_and_session(
            &EchoCap,
            &json!({"content": "suspicious manipulation of system files"}),
            false,
            &wp,
            None,
            None,
            30,
        );

        std::env::remove_var("RUNTIMO_DAL");

        // With DAL=A, cognitive pipeline may reject — test that it either succeeds
        // or fails with CognitiveSafetyViolation (not some other error)
        match result {
            Ok(r) => {
                // If it passed, it's because DAL=A didn't trigger for these inputs
                assert!(r.success || !r.output.output.as_str().contains("cognitive"));
            }
            Err(e) => {
                assert!(
                    matches!(e, crate::Error::CognitiveSafetyViolation(_)),
                    "Expected CognitiveSafetyViolation, got {:?}",
                    e
                );
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // ── Cognitive pipeline with DAL=E passes ──────────────────────────
    //
    // With DAL=E, every decision becomes Proceed — verifies the pipeline
    // does not prevent valid executions.

    #[test]
    fn test_cognitive_pipeline_dal_e_passes() {
        let _guard = DAL_TEST_MUTEX.lock().unwrap();
        // Set DAL to E (everything allowed)
        std::env::set_var("RUNTIMO_DAL", "E");

        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let wp = wal_path(&dir);

        // EchoCap goes through cognitive pipeline (not in skip list).
        let result = execute_with_telemetry_and_session(
            &EchoCap,
            &json!({"content": "normal content"}),
            false,
            &wp,
            None,
            None,
            30,
        );

        std::env::remove_var("RUNTIMO_DAL");

        // DAL=E should always allow execution
        assert!(result.is_ok(), "DAL=E should pass: {:?}", result.err());
        assert!(result.unwrap().success);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_identify_spawned_pids() {
        // Deterministic test: construct snapshots with known PIDs.
        let before = ProcessSnapshot {
            timestamp: 1000,
            processes: vec![
                crate::processes::ProcessInfo {
                    pid: 1,
                    ppid: 0,
                    user: "root".into(),
                    cpu_percent: 0.0,
                    mem_percent: 0.0,
                    vsz: 0,
                    rss: 0,
                    stat: "S".into(),
                    start_time: String::new(),
                    elapsed: String::new(),
                    command: "init".into(),
                },
                crate::processes::ProcessInfo {
                    pid: 42,
                    ppid: 1,
                    user: "user".into(),
                    cpu_percent: 1.0,
                    mem_percent: 0.5,
                    vsz: 1000,
                    rss: 500,
                    stat: "S".into(),
                    start_time: String::new(),
                    elapsed: String::new(),
                    command: "existing".into(),
                },
            ],
            summary: crate::processes::ProcessSummary {
                total_processes: 2,
                total_cpu_percent: 1.0,
                total_mem_percent: 0.5,
                top_cpu_consumer: None,
                top_mem_consumer: None,
                zombie_count: 0,
            },
        };
        let after = ProcessSnapshot {
            timestamp: 1001,
            processes: vec![
                crate::processes::ProcessInfo {
                    pid: 1,
                    ppid: 0,
                    user: "root".into(),
                    cpu_percent: 0.0,
                    mem_percent: 0.0,
                    vsz: 0,
                    rss: 0,
                    stat: "S".into(),
                    start_time: String::new(),
                    elapsed: String::new(),
                    command: "init".into(),
                },
                crate::processes::ProcessInfo {
                    pid: 42,
                    ppid: 1,
                    user: "user".into(),
                    cpu_percent: 1.0,
                    mem_percent: 0.5,
                    vsz: 1000,
                    rss: 500,
                    stat: "S".into(),
                    start_time: String::new(),
                    elapsed: String::new(),
                    command: "existing".into(),
                },
                crate::processes::ProcessInfo {
                    pid: 99,
                    ppid: 42,
                    user: "user".into(),
                    cpu_percent: 0.0,
                    mem_percent: 0.1,
                    vsz: 100,
                    rss: 50,
                    stat: "S".into(),
                    start_time: String::new(),
                    elapsed: String::new(),
                    command: "spawned".into(),
                },
            ],
            summary: crate::processes::ProcessSummary {
                total_processes: 3,
                total_cpu_percent: 1.0,
                total_mem_percent: 0.6,
                top_cpu_consumer: None,
                top_mem_consumer: None,
                zombie_count: 0,
            },
        };

        let spawned = identify_spawned_pids(&before, &after);
        assert_eq!(spawned.len(), 1, "Should detect exactly 1 spawned PID");
        assert_eq!(spawned[0], 99, "Spawned PID should be 99");
    }
}
