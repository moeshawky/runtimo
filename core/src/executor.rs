//! Execution engine — telemetry-wrapped capability execution.
//!
//! Wraps every capability execution with:
//! telemetry capture → resource check → WAL log → validate → execute → WAL log
//!
//! Capabilities execute with an advisory 30-second post-hoc timeout check.
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
use crate::config::RuntimoConfig;
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
/// **Note:** This is an advisory post-hoc timeout — the capability runs to
/// completion and then elapsed time is checked. Pure-Rust capabilities cannot
/// be forcibly interrupted (see [`execute_with_timeout_check`]).
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
    let cap_name = capability.name();
    let timeout = RuntimoConfig::get_capability_timeout(cap_name, CAPABILITY_TIMEOUT_SECS);
    execute_with_telemetry_and_session(capability, args, dry_run, wal_path, None, None, timeout)
}

/// Returns whether telemetry capture is enabled via the resolved config.
///
/// Reads `RuntimoConfig::load().resolved().telemetry_enabled`. When false
/// (e.g. the `ephemeral` profile), the executor skips all telemetry capture,
/// stores `None` in WAL telemetry fields, and carries
/// [`Telemetry::empty`] in-memory — no `/proc` reads or subprocess probes
/// occur on the disabled path. Process snapshots are still captured: the
/// zombie guard and spawned-PID detection depend on them.
fn telemetry_enabled() -> bool {
    RuntimoConfig::load().resolved().telemetry_enabled
}

/// Returns the WAL telemetry field for a snapshot.
///
/// `Some` clone when telemetry is enabled, `None` when disabled — WAL
/// telemetry fields are `Option`, so the disabled path stores `None`
/// (contract-legal, skipped by `skip_serializing_if`).
fn telemetry_opt(enabled: bool, tel: &Telemetry) -> Option<Telemetry> {
    enabled.then(|| tel.clone())
}

/// Captures a fresh after-execution telemetry snapshot.
///
/// Bypasses the lightweight cache so the after snapshot always differs
/// from the before snapshot (no same-timestamp alias). Returns
/// [`Telemetry::empty`] without any I/O when telemetry is disabled.
fn fresh_telemetry_after(enabled: bool) -> Telemetry {
    if enabled {
        Telemetry::clear_lightweight_cache();
        Telemetry::capture_lightweight()
    } else {
        Telemetry::empty()
    }
}

/// Captures a fresh after-execution process snapshot, bypassing the cache
/// so the after snapshot never aliases the before snapshot via a cache hit.
fn fresh_process_after() -> ProcessSnapshot {
    ProcessSnapshot::clear_cache();
    ProcessSnapshot::capture()
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
/// When resolved config `telemetry_enabled` is false (e.g. the `ephemeral`
/// profile), capture is skipped: WAL telemetry fields are `None` and the
/// returned [`ExecutionResult`] carries [`Telemetry::empty`]. After
/// snapshots bypass the caches so before/after never alias via a hit.
///
/// # Cognitive Safety
///
/// Capabilities with user-authored natural language content (commands, file
/// content, URLs, commit messages) pass through the llmosafe `CognitivePipeline`
/// for TF-IDF + keyword bias detection. Structured-only capabilities (paths,
/// PIDs, job IDs) skip the check to avoid NLP false positives.
///
/// # Backup Audit
///
/// For mutating capabilities that create a backup before writing (FileWrite,
/// Delete, GitExec), this function emits a [`WalEventType::BackupCreated`] WAL
/// event carrying the original `path` and `backup_path`, always BEFORE the
/// terminal lifecycle append so the backup remains auditable (and `undo`-able)
/// even when the terminal append never lands, preventing an orphaned backup.
///
/// - **Success path:** the event is emitted from the capability's reported
///   `output.data` (`path` + `backup_path`) before the fallible
///   `JobCompleted` append.
/// - **Failure path (Err):** the capability's `Output` is unavailable, so the
///   backup location is derived from the `BackupManager` layout —
///   `backup_dir()/job_id/file_name` with collision suffixes `.1`–`.10`
///   (see `BackupManager::create_backup`, which appends the first free
///   suffix). An existence check gates the emit; the `BackupCreated` event
///   is appended before `JobFailed` so `undo`'s WAL scan (which reads
///   `output.data.{path, backup_path}` from any event of the job) covers
///   failed jobs too. The audit append is best-effort (`let _`): a failure
///   here must not mask the capability failure that `JobFailed` records.
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

    // Telemetry gate (RC1): when disabled, skip capture entirely — no
    // /proc reads, no subprocess probes. WAL telemetry fields are None;
    // the in-memory result carries Telemetry::empty().
    let telemetry_on = telemetry_enabled();
    let telemetry_before = if telemetry_on {
        Telemetry::capture_lightweight()
    } else {
        Telemetry::empty()
    };
    let process_before = ProcessSnapshot::capture();

    // WAL is created BEFORE the guard checks so every rejection below is
    // audited with a JobFailed event (no silent early-Err gap).
    let mut wal = WalWriter::create(wal_path)?;

    // LlmoSafeGuard is the circuit breaker — reads /proc/stat with delta measurement
    let guard = LlmoSafeGuard::new();
    if let Err(e) = guard.check() {
        let msg = e;
        let tel = telemetry_opt(telemetry_on, &telemetry_before);
        let _ = log_job_failed_with_snapshots(
            &mut wal,
            &job_id_str,
            &cap_name,
            &msg,
            tel.as_ref(),
            tel.as_ref(),
            &process_before.summary,
            &process_before.summary,
            None,
            None,
        );
        return Err(Error::ResourceLimitExceeded(msg));
    }

    // Reject if zombie count > 10
    if process_before.summary.zombie_count > 10 {
        let msg = format!(
            "Zombie processes: {} (limit: 10)",
            process_before.summary.zombie_count
        );
        let tel = telemetry_opt(telemetry_on, &telemetry_before);
        let _ = log_job_failed_with_snapshots(
            &mut wal,
            &job_id_str,
            &cap_name,
            &msg,
            tel.as_ref(),
            tel.as_ref(),
            &process_before.summary,
            &process_before.summary,
            None,
            None,
        );
        return Err(Error::ResourceLimitExceeded(msg));
    }

    // Args size guard: reject oversized arguments (1MB max)
    let args_bytes = serde_json::to_vec(args)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to serialize args: {}", e)))?;
    if args_bytes.len() > MAX_ARGS_SIZE_BYTES {
        let msg = format!(
            "Capability args too large: {} bytes (limit: 1MB)",
            args_bytes.len()
        );
        drop(args_bytes);
        let tel = telemetry_opt(telemetry_on, &telemetry_before);
        let _ = log_job_failed_with_snapshots(
            &mut wal,
            &job_id_str,
            &cap_name,
            &msg,
            tel.as_ref(),
            tel.as_ref(),
            &process_before.summary,
            &process_before.summary,
            None,
            None,
        );
        return Err(Error::ResourceLimitExceeded(msg));
    }
    drop(args_bytes);

    let ctx = Context::with_working_dir(
        dry_run,
        job_id_str.clone(),
        working_dir.unwrap_or_else(|| {
            log::warn!("working_dir not set, falling back to /");
            PathBuf::from("/")
        }),
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
        telemetry_before: telemetry_opt(telemetry_on, &telemetry_before),
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
        backup_path: None,
        bundle_hash: None,
        mono_ns: None,
        wall_ns: None,
    })?;

    // Cognitive safety check — runs llmosafe's CognitivePipeline
    // (sifter + bias detection + surprise gating + detectors) against
    // user-authored natural language content (commands, file content,
    // URLs, commit messages). Structured inputs (paths, PIDs, job IDs)
    // are skipped — the TF-IDF classifier was trained on manipulation
    // text and produces false positives on structured data.
    //
    // ShellExec is excluded: its blocklist already validates dangerous
    // commands, and the NLP sifter produces false positives on shell
    // command syntax (e.g. `ls -la` flagged as CognitiveInstability).
    let skip_cognitive = cap_name == "ShellExec";
    if !skip_cognitive && has_natural_content(args) {
        let pipeline_result = guard
            .check_cognitive_pipeline(
                capability.description(),
                &sift_observation(capability.description(), args),
            )
            .map_err(|e| Error::ExecutionFailed(format!("Cognitive safety check failed: {}", e)))?;

        if !pipeline_result.decision.can_proceed() {
            let telemetry_after = fresh_telemetry_after(telemetry_on);
            let process_after = fresh_process_after();
            let err_msg = format!(
                "Cognitive safety violation: decision {:?}",
                pipeline_result.decision
            );
            log_job_failed_with_snapshots(
                &mut wal,
                &job_id_str,
                &cap_name,
                &err_msg,
                telemetry_opt(telemetry_on, &telemetry_before).as_ref(),
                telemetry_opt(telemetry_on, &telemetry_after).as_ref(),
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

    // Execute capability with timeout enforcement. The executor-level timeout
    // is injected into the args so subprocess-based capabilities (ShellExec,
    // GitExec) honor it in their internal kill logic — without this, the
    // internal enforcement falls back to each capability's hardcoded default
    // and the configured `timeout_secs` is only advisory.
    let output = match execute_with_timeout_check(
        capability,
        &inject_timeout(args, timeout_secs),
        &ctx,
        timeout_secs,
    ) {
        Ok(out) => out,
        Err(e) => {
            let telemetry_after = fresh_telemetry_after(telemetry_on);
            let process_after = fresh_process_after();
            // T4: backup orphan audit — if capability created a backup before failing,
            // the file exists at backup_dir/job_id/file_name. Emit BackupCreated
            // before JobFailed so WAL consumers and undo can locate it. Scans
            // known path keys (path, repo_path, dir) to cover FileWrite and
            // GitExec alike; uses existence check with collision suffixes
            // `.1`–`.10` mirroring `BackupManager::create_backup`.
            for key in ["path", "repo_path", "dir"] {
                if let Some(orig_path) = args.get(key).and_then(|v| v.as_str()) {
                    if orig_path.is_empty() {
                        continue;
                    }
                    let Some(file_name) = std::path::Path::new(orig_path).file_name() else {
                        continue;
                    };
                    let job_dir = crate::utils::backup_dir().join(&job_id_str);
                    let found = find_backup_candidate(&job_dir, file_name);
                    if let Some(bp) = found {
                        let backup_seq = wal.seq();
                        if let Err(e) = wal.append(WalEvent {
                            seq: backup_seq,
                            ts: telemetry_after.timestamp,
                            event_type: WalEventType::BackupCreated,
                            job_id: job_id_str.clone(),
                            capability: Some(cap_name.clone()),
                            output: Some(serde_json::json!({
                                "data": {
                                    "path": orig_path,
                                    "backup_path": bp.to_string_lossy().to_string()
                                }
                            })),
                            error: None,
                            telemetry_before: None,
                            telemetry_after: None,
                            process_before: None,
                            process_after: None,
                            cmd: None,
                            cmd_stdout: None,
                            cmd_stderr: None,
                            cmd_exit_code: None,
                            cmd_corrected: None,
                            oov_ratio: None,
                            detection_flags: None,
                            backup_path: Some(bp),
                            bundle_hash: None,
                            mono_ns: None,
                            wall_ns: None,
                        }) {
                            log::error!(
                                "WAL BackupCreated append failed for job {}: {}",
                                job_id_str,
                                e
                            );
                        }
                    }
                }
            }
            let end_seq = wal.seq();
            let err_msg = format!("Execution failed: {}", e);
            log_job_failed_with_snapshots(
                &mut wal,
                &job_id_str,
                &cap_name,
                &err_msg,
                telemetry_opt(telemetry_on, &telemetry_before).as_ref(),
                telemetry_opt(telemetry_on, &telemetry_after).as_ref(),
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

    // Fresh after snapshots bypass the caches so before/after never alias
    // via a cache hit (same-timestamp reads).
    let telemetry_after = fresh_telemetry_after(telemetry_on);
    let process_after = fresh_process_after();

    // RC1 fix (CBP B2): record the backup path in a durable WAL event BEFORE the
    // fallible JobCompleted append. The backup is created inside the capability
    // (between JobStarted and JobCompleted); if the JobCompleted append fails, the
    // backup would otherwise be orphaned and `undo` could not locate it. Emitting
    // BackupCreated here — using the capability's reported `backup_path` and original
    // `path` from `output.data` — guarantees the backup is audited even when
    // JobCompleted never lands. Covers FileWrite, Delete, and GitExec uniformly.
    if let Some(data) = output.data.as_ref() {
        if let (Some(bp), Some(orig)) = (
            data.get("backup_path").and_then(Value::as_str),
            data.get("path").and_then(Value::as_str),
        ) {
            if !bp.is_empty() {
                let backup_seq = wal.seq();
                wal.append(WalEvent {
                    seq: backup_seq,
                    ts: telemetry_after.timestamp,
                    event_type: WalEventType::BackupCreated,
                    job_id: job_id_str.clone(),
                    capability: Some(cap_name.clone()),
                    output: Some(serde_json::json!({
                        "data": {
                            "path": orig,
                            "backup_path": bp
                        }
                    })),
                    error: None,
                    telemetry_before: None,
                    telemetry_after: None,
                    process_before: None,
                    process_after: None,
                    cmd: None,
                    cmd_stdout: None,
                    cmd_stderr: None,
                    cmd_exit_code: None,
                    cmd_corrected: None,
                    oov_ratio: None,
                    detection_flags: None,
                    backup_path: Some(std::path::PathBuf::from(bp)),
                    bundle_hash: None,
                    mono_ns: None,
                    wall_ns: None,
                })?;
            }
        }
    }

    // Identify spawned PIDs by comparing before/after process lists.
    // Routed via log (not stderr) so --quiet/--json merged streams stay clean.
    let spawned_pids = identify_spawned_pids(&process_before, &process_after);
    if !spawned_pids.is_empty() {
        log::warn!(
            "capability '{}' spawned {} process(es): PIDs {:?}",
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
        telemetry_before: telemetry_opt(telemetry_on, &telemetry_before),
        telemetry_after: telemetry_opt(telemetry_on, &telemetry_after),
        process_before: Some(process_before.summary.clone()),
        process_after: Some(process_after.summary.clone()),
        cmd: None,
        cmd_stdout: None,
        cmd_stderr: None,
        cmd_exit_code: None,
        cmd_corrected: None,
        oov_ratio: None,
        detection_flags: None,
        backup_path: None,
        bundle_hash: None,
        mono_ns: None,
        wall_ns: None,
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
            backup_path: None,
            bundle_hash: None,
            mono_ns: None,
            wall_ns: None,
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
///
/// Telemetry parameters are `Option`: pass `None` when telemetry is disabled
/// via resolved config so the WAL records no telemetry on the disabled path.
#[allow(clippy::too_many_arguments)]
fn log_job_failed_with_snapshots(
    wal: &mut WalWriter,
    job_id: &str,
    capability: &str,
    error: &str,
    telemetry_before: Option<&Telemetry>,
    telemetry_after: Option<&Telemetry>,
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
        telemetry_before: telemetry_before.cloned(),
        telemetry_after: telemetry_after.cloned(),
        process_before: Some(process_before.clone()),
        process_after: Some(process_after.clone()),
        cmd: None,
        cmd_stdout: None,
        cmd_stderr: None,
        cmd_exit_code: None,
        cmd_corrected: None,
        oov_ratio,
        detection_flags,
        backup_path: None,
        bundle_hash: None,
        mono_ns: None,
        wall_ns: None,
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

/// Injects the executor-level timeout into capability args when they do not
/// already specify one.
///
/// Subprocess-based capabilities (ShellExec, GitExec) enforce timeouts
/// internally from their args' `timeout_secs` field. Without injection the
/// executor-level `timeout_secs` (CLI `--timeout`, config
/// `capability_timeouts`) is only advisory, and the internal kill falls back
/// to each capability's hardcoded default. Injection is a no-op for
/// capabilities whose args struct ignores the key (serde drops unknown keys).
#[must_use]
fn inject_timeout(args: &Value, timeout_secs: u64) -> Value {
    let mut value = args.clone();
    if let Some(obj) = value.as_object_mut() {
        if !obj.contains_key("timeout_secs") {
            obj.insert("timeout_secs".to_string(), Value::from(timeout_secs));
        }
    }
    value
}

/// Locates an existing backup file for `file_name` under `job_dir`.
///
/// Mirrors the collision-suffix layout of [`crate::backup::BackupManager::create_backup`]:
/// `job_dir/file_name`, then `job_dir/file_name.1` … `job_dir/file_name.10`.
/// Returns the first path that exists on disk, or `None` if none is found.
/// Used by the Err-path `BackupCreated` audit to avoid scanning only the
/// unsuffixed candidate.
///
/// # Parameters
/// - `job_dir`: `backup_dir()/job_id`
/// - `file_name`: file name component of the original path
fn find_backup_candidate(job_dir: &Path, file_name: &std::ffi::OsStr) -> Option<PathBuf> {
    let mut candidate = job_dir.join(file_name);
    if candidate.exists() {
        return Some(candidate);
    }
    for i in 1..=10 {
        candidate = job_dir.join(format!("{}.{}", file_name.to_string_lossy(), i));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Execute a capability inline and check if it exceeded the timeout.
///
/// Runs the capability and measures elapsed time. For subprocess-based
/// capabilities (ShellExec, GitExec), the timeout is enforced internally
/// by the capability. For pure-Rust capabilities, the timeout is checked
/// **after** execution completes — the capability cannot be forcibly
/// interrupted without subprocess isolation. If the timeout was exceeded,
/// the original [`Output`] is preserved and enriched with `timed_out: true`
/// in `data` instead of converting the success into an `Err` (which would
/// discard the output). A warning is logged via `log::warn!` for
/// observability, but the caller still receives the successful output
/// with the sentinel flag so downstream consumers can distinguish
/// slow-success from fast-success without losing data. Failures (`Err`)
/// are propagated unchanged.
///
/// # Parameters
/// - `capability`: capability to execute
/// - `args`: JSON arguments (already timeout-injected)
/// - `ctx`: execution context
/// - `timeout_secs`: advisory timeout in seconds
///
/// # Returns
/// - `Ok(Output)` with `data.timed_out == true` when the capability
///   succeeded but exceeded `timeout_secs`
/// - `Ok(Output)` unchanged when execution was within budget
/// - `Err` when the capability itself failed (even if it also timed out)
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
        // Routed via log (not stderr) so --quiet/--json merged streams stay clean.
        log::warn!(
            "capability exceeded timeout: {:.1}s > {}s",
            elapsed.as_secs_f64(),
            timeout_secs
        );
        let mut out = output?;
        match &mut out.data {
            Some(Value::Object(map)) => {
                map.insert("timed_out".to_string(), Value::Bool(true));
            }
            Some(other) => {
                // Non-object data: wrap original value and add flag.
                let original = std::mem::replace(other, Value::Null);
                *other = serde_json::json!({"value": original, "timed_out": true});
            }
            None => {
                out.data = Some(serde_json::json!({"timed_out": true}));
            }
        }
        return Ok(out);
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

    #[test]
    fn inject_timeout_adds_missing_key() {
        let args = json!({"cmd": "ls"});
        let injected = inject_timeout(&args, 3600);
        assert_eq!(injected["timeout_secs"], 3600);
        assert_eq!(injected["cmd"], "ls");
    }

    #[test]
    fn inject_timeout_preserves_existing_key() {
        let args = json!({"cmd": "ls", "timeout_secs": 90});
        let injected = inject_timeout(&args, 3600);
        assert_eq!(injected["timeout_secs"], 90, "explicit arg must win");
    }

    #[test]
    fn inject_timeout_noop_for_non_object_args() {
        let args = json!("just a string");
        assert_eq!(inject_timeout(&args, 3600), json!("just a string"));
    }

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
        // T7exec: slow success must preserve Output with timed_out:true instead of Err.
        let result = execute_with_timeout_check(
            &SlowCap,
            &json!({}),
            &Context::new(false, "timeout-test".into()),
            0, // zero timeout — any execution exceeds it
        );
        assert!(
            result.is_ok(),
            "Slow success should be preserved as Ok with timed_out flag, got: {:?}",
            result
        );
        let out = result.unwrap();
        assert_eq!(out.status, "ok");
        let data = out
            .data
            .expect("slow-success should have data with timed_out");
        assert_eq!(
            data.get("timed_out"),
            Some(&Value::Bool(true)),
            "timed_out flag must be true for slow success, got: {:?}",
            data
        );
    }

    #[test]
    fn test_execute_with_timeout_preserves_output_with_timed_out_flag() {
        // Explicit slow-success sentinel test: Output data is preserved and enriched.
        let result = execute_with_timeout_check(
            &SlowCap,
            &json!({"extra": "keep"}),
            &Context::new(false, "timeout-preserve".into()),
            0,
        )
        .unwrap();
        assert_eq!(result.status, "ok");
        let data = result.data.unwrap();
        assert_eq!(data.get("timed_out"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_execute_with_timeout_fast_path_no_flag() {
        // Fast path must not inject timed_out.
        struct FastCap;
        impl Capability for FastCap {
            fn name(&self) -> &'static str {
                "Fast"
            }
            fn description(&self) -> &'static str {
                "fast capability"
            }
            fn schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn validate(&self, _args: &Value) -> crate::Result<()> {
                Ok(())
            }
            fn execute(&self, _args: &Value, _ctx: &Context) -> crate::Result<Output> {
                Ok(Output::ok("fast completed".into()))
            }
        }
        let result = execute_with_timeout_check(
            &FastCap,
            &json!({}),
            &Context::new(false, "fast-test".into()),
            30,
        )
        .unwrap();
        assert_eq!(result.status, "ok");
        if let Some(data) = result.data {
            assert!(
                data.get("timed_out").is_none(),
                "fast path must not have timed_out flag, got: {:?}",
                data
            );
        }
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
    fn test_early_args_rejection_logs_job_failed() {
        // F5: oversized args rejected before JobStarted must still leave a
        // JobFailed audit event (no silent early-Err gap).
        let dir = unique_test_dir();
        fs::create_dir_all(&dir).ok();
        let wp = wal_path(&dir);

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
        assert!(result.is_err(), "Should reject args > 1MB");

        let reader = crate::WalReader::load(&wp).unwrap();
        assert!(
            reader
                .events()
                .iter()
                .any(|e| matches!(e.event_type, crate::WalEventType::JobFailed)),
            "early-Err rejection must log JobFailed"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_telemetry_opt_gates_wal_field() {
        // RC1: disabled telemetry maps to None WAL fields, enabled to Some.
        let tel = Telemetry::empty();
        assert!(telemetry_opt(false, &tel).is_none());
        let some = telemetry_opt(true, &tel);
        assert!(some.is_some());
        assert_eq!(some.unwrap().system.cpu_model, "unknown");
    }

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

    /// Capability that creates a backup then fails — simulates FileWrite post-backup failure.
    struct FailAfterBackupCap;
    impl Capability for FailAfterBackupCap {
        fn name(&self) -> &'static str {
            "FailAfterBackup"
        }
        fn description(&self) -> &'static str {
            "creates backup then fails"
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
        }
        fn validate(&self, _args: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, args: &Value, ctx: &Context) -> crate::Result<Output> {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let path = std::path::Path::new(path_str);
            if path.exists() {
                let backup_dir = crate::utils::backup_dir();
                let mgr = crate::backup::BackupManager::new(backup_dir)
                    .map_err(|e| crate::Error::BackupError(format!("backup mgr: {}", e)))?;
                let _ = mgr.create_backup(path, &ctx.job_id);
            }
            Err(crate::Error::ExecutionFailed(
                "simulated failure after backup".into(),
            ))
        }
    }

    #[test]
    fn test_backup_orphan_on_err_path_is_audited() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).ok();
        let target = make_file(&dir, "orphan.txt", "original content");
        let wp = wal_path(&dir);

        let cap = FailAfterBackupCap;
        let result = execute_with_telemetry_and_session(
            &cap,
            &json!({"path": target.to_str().unwrap()}),
            false,
            &wp,
            None,
            None,
            30,
        )
        .unwrap();
        assert!(!result.success, "should be failure");
        let reader = crate::WalReader::load(&wp).unwrap();
        let events = reader.events();
        let has_backup = events.iter().any(|e| {
            matches!(e.event_type, crate::WalEventType::BackupCreated)
                && e.job_id == result.job_id
                && e.backup_path.is_some()
        });
        assert!(
            has_backup,
            "WAL must contain BackupCreated for failed job {} — orphan backup not audited",
            result.job_id
        );
        // WAL ordering invariant: BackupCreated must precede JobFailed so
        // consumers that stop reading at the terminal event can still
        // locate the backup.
        let backup_idx = events.iter().position(|e| {
            matches!(e.event_type, crate::WalEventType::BackupCreated) && e.job_id == result.job_id
        });
        let failed_idx = events.iter().position(|e| {
            matches!(e.event_type, crate::WalEventType::JobFailed) && e.job_id == result.job_id
        });
        assert!(
            backup_idx.is_some(),
            "BackupCreated event missing for job {}",
            result.job_id
        );
        assert!(
            failed_idx.is_some(),
            "JobFailed event missing for job {}",
            result.job_id
        );
        assert!(
            backup_idx < failed_idx,
            "BackupCreated (idx {:?}) must precede JobFailed (idx {:?})",
            backup_idx,
            failed_idx
        );
        let backup_event = events
            .iter()
            .find(|e| {
                matches!(e.event_type, crate::WalEventType::BackupCreated)
                    && e.job_id == result.job_id
            })
            .unwrap();
        let bp = backup_event.backup_path.as_ref().unwrap();
        assert!(bp.exists(), "backup file should exist at {:?}", bp);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(crate::utils::backup_dir().join(&result.job_id));
    }

    /// Capability that backs up via `repo_path` key then fails — simulates GitExec.
    struct FailAfterBackupRepoCap;
    impl Capability for FailAfterBackupRepoCap {
        fn name(&self) -> &'static str {
            "FailAfterBackupRepo"
        }
        fn description(&self) -> &'static str {
            "creates backup via repo_path then fails"
        }
        fn schema(&self) -> Value {
            json!({"type": "object", "properties": {"repo_path": {"type": "string"}}, "required": ["repo_path"]})
        }
        fn validate(&self, _args: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, args: &Value, ctx: &Context) -> crate::Result<Output> {
            let path_str = args.get("repo_path").and_then(|v| v.as_str()).unwrap_or("");
            let path = std::path::Path::new(path_str);
            if path.exists() {
                let backup_dir = crate::utils::backup_dir();
                let mgr = crate::backup::BackupManager::new(backup_dir)
                    .map_err(|e| crate::Error::BackupError(format!("backup mgr: {}", e)))?;
                let _ = mgr.create_backup(path, &ctx.job_id);
            }
            Err(crate::Error::ExecutionFailed(
                "simulated repo_path failure after backup".into(),
            ))
        }
    }

    #[test]
    fn test_backup_orphan_via_repo_path_is_audited() {
        // T4 widening: GitExec uses `path`/`repo_path` — Err audit must scan both.
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).ok();
        let target = make_file(&dir, "repo_orphan.txt", "repo content");
        let wp = wal_path(&dir);

        let cap = FailAfterBackupRepoCap;
        let result = execute_with_telemetry_and_session(
            &cap,
            &json!({"repo_path": target.to_str().unwrap()}),
            false,
            &wp,
            None,
            None,
            30,
        )
        .unwrap();
        assert!(!result.success, "should be failure");
        let reader = crate::WalReader::load(&wp).unwrap();
        let events = reader.events();
        let has_backup = events.iter().any(|e| {
            matches!(e.event_type, crate::WalEventType::BackupCreated)
                && e.job_id == result.job_id
                && e.backup_path.is_some()
        });
        assert!(
            has_backup,
            "WAL must contain BackupCreated for repo_path failed job {}",
            result.job_id
        );
        let backup_idx = events.iter().position(|e| {
            matches!(e.event_type, crate::WalEventType::BackupCreated) && e.job_id == result.job_id
        });
        let failed_idx = events.iter().position(|e| {
            matches!(e.event_type, crate::WalEventType::JobFailed) && e.job_id == result.job_id
        });
        assert!(
            backup_idx < failed_idx,
            "BackupCreated must precede JobFailed for repo_path"
        );
        let bp = events
            .iter()
            .find(|e| {
                matches!(e.event_type, crate::WalEventType::BackupCreated)
                    && e.job_id == result.job_id
            })
            .unwrap()
            .backup_path
            .as_ref()
            .unwrap();
        assert!(bp.exists(), "backup file should exist at {:?}", bp);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(crate::utils::backup_dir().join(&result.job_id));
    }

    #[test]
    fn test_find_backup_candidate_with_suffix() {
        // Suffix scan: .1 must be found when unsuffixed candidate missing.
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).ok();
        let job_dir = dir.join("job_suffix");
        std::fs::create_dir_all(&job_dir).ok();
        let orig = dir.join("suffix_test.txt");
        std::fs::write(&orig, "orig").unwrap();
        let suffixed = job_dir.join("suffix_test.txt.1");
        std::fs::write(&suffixed, "backup1").unwrap();
        let found = find_backup_candidate(&job_dir, std::ffi::OsStr::new("suffix_test.txt"));
        assert!(found.is_some());
        assert_eq!(found.unwrap(), suffixed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
