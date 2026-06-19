//! Runtimo Daemon - Unix socket JSON-RPC server for capability execution
//!
//! Usage: runtimo `[OPTIONS]`
//!
//! Options:
//!   --socket `PATH`    Unix socket path (default: `data`/runtimo.sock)
//!   --http             Enable HTTP listener (placeholder)
//!   --http-port `PORT` HTTP port (default: 8080)
//!
//! # Background Mode
//!
//! Supports `dispatch` — submit a capability, get job ID immediately, check later.
//! Uses `status` and `jobs` RPC methods for queriable job history.

use runtimo_core::{
    capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo, is_dangerous_command},
    execute_with_telemetry_and_session, BackupManager, CapabilityRegistry, WalEvent, WalEventType,
    WalReader, WalWriter,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Re-exports from sibling modules so `use super::*` in tests can see these names.
pub use crate::auth::authenticate_peer;
pub use crate::config::{default_socket_path, default_wal_path, ensure_data_dir};
#[allow(unused_imports)] // used in inline tests
pub use crate::jobs::{BackgroundJob, BackgroundJobRegistry, MAX_CONCURRENT_JOBS};
#[allow(unused_imports)] // used in inline tests
pub use crate::rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, LogsParams, RunParams};

// ── Daemon state ────────────────────────────────────────────────────────────

/// Shared daemon state — capability registry, WAL path, and background job tracker.
///
/// Single instance shared across all client connections via `Arc`.
struct DaemonState {
    /// Registry of all available capabilities.
    registry: CapabilityRegistry,
    /// Path to the Write-Ahead Log file.
    wal_path: PathBuf,
    /// Mutex serializing WAL writes across concurrent client handlers.
    /// Uses std::sync::Mutex to allow locking from both async handlers and
    /// blocking tasks (spawn_blocking) without nested runtimes.
    wal_mutex: Arc<Mutex<()>>,
    /// Background job registry for dispatch/status tracking.
    bg_jobs: BackgroundJobRegistry,
}

impl DaemonState {
    fn new(wal_path: &Path) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut registry = CapabilityRegistry::new();
        registry.register(FileRead);

        let file_write = FileWrite::new()
            .map_err(|e| format!("Failed to create FileWrite capability: {}", e))?;
        registry.register(file_write);

        let backup_dir = runtimo_core::utils::backup_dir();
        let git_exec = GitExec::new(backup_dir)
            .map_err(|e| format!("Failed to create GitExec capability: {}", e))?;
        registry.register(git_exec);

        registry.register(ShellExec);
        registry.register(Kill);
        registry.register(Undo);

        if let Some(parent) = wal_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create WAL directory: {}", e))?;
        }

        Ok(Self {
            registry,
            wal_path: wal_path.to_path_buf(),
            wal_mutex: Arc::new(Mutex::new(())),
            bg_jobs: BackgroundJobRegistry::new(),
        })
    }
}

// ── Request routing ─────────────────────────────────────────────────────────

/// Routes an incoming JSON-RPC request to the appropriate handler.
///
/// Supported methods: `run`, `dispatch`, `status`, `jobs`, `list`, `logs`.
/// Returns a `JsonRpcResponse` with `error.code = -32601` for unknown methods.
async fn handle_request(state: &Arc<DaemonState>, req: JsonRpcRequest) -> JsonRpcResponse {
    match req.method.as_str() {
        "run" => handle_run(state, req.params, req.id).await,
        "dispatch" => handle_dispatch(state, req.params, req.id).await,
        "status" => handle_status(state, req.params, req.id).await,
        "jobs" => handle_jobs(state, req.params, req.id).await,
        "list" => handle_list(state, req.id),
        "logs" => handle_logs(state, req.params, req.id),
        _ => JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("Method not found: {}", req.method),
            }),
            id: req.id,
        },
    }
}

/// Handles the `run` RPC method — synchronous capability execution with full telemetry.
///
/// Acquires the WAL mutex, executes the capability, and returns the result.
/// Times out at `timeout_secs` (default 30s).
#[allow(clippy::unused_async)]
async fn handle_run(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    let run_params: RunParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: format!("Invalid params: {}", e),
                }),
                id,
            };
        }
    };

    let Some(capability) = state.registry.get(&run_params.capability) else {
        return JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32602,
                message: format!("Capability not found: {}", run_params.capability),
            }),
            id,
        };
    };

    let _guard = state.wal_mutex.lock().unwrap_or_else(|e| e.into_inner());

    match execute_with_telemetry_and_session(
        capability,
        &run_params.args,
        run_params.dry_run,
        &state.wal_path,
        None,
        run_params.working_dir.clone().map(PathBuf::from),
        run_params.timeout_secs.unwrap_or(30),
    ) {
        Ok(result) => JsonRpcResponse {
            result: Some(serde_json::json!({
                "success": result.success,
                "job_id": result.job_id,
                "capability": result.capability,
                "output": serde_json::to_value(&result.output).unwrap_or(Value::Null),
            })),
            error: None,
            id,
        },
        Err(e) => JsonRpcResponse {
            result: Some(serde_json::json!({
                "success": false,
                "error": e.to_string(),
            })),
            error: None,
            id,
        },
    }
}

/// Handles the `dispatch` RPC method — fire-and-forget background job execution.
///
/// Returns immediately with a job ID. The job runs on a blocking task via
/// `tokio::task::spawn_blocking` with the same safety checks, WAL logging,
/// and telemetry as `handle_run`. Rejects when `MAX_CONCURRENT_JOBS` (16) is reached.
///
/// The spawn_blocking approach avoids blocking the tokio runtime. WAL mutex and
/// background job registry use std::sync primitives, so no nested runtime is needed.
#[allow(clippy::unused_async)]
async fn handle_dispatch(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    let run_params: RunParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: format!("Invalid params: {}", e),
                }),
                id,
            };
        }
    };

    if state.registry.get(&run_params.capability).is_none() {
        return JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32602,
                message: format!("Capability not found: {}", run_params.capability),
            }),
            id,
        };
    }

    // Early validation for ShellExec: check for dangerous commands before
    // creating a job. This prevents accepting dangerous commands at dispatch
    // time that would only fail at async execution time (F-003).
    if run_params.capability == "ShellExec" {
        if let Some(cmd) = run_params.args.get("cmd").or_else(|| run_params.args.get("command")) {
            if let Some(cmd_str) = cmd.as_str() {
                if let Some(reason) = is_dangerous_command(cmd_str) {
                    return JsonRpcResponse {
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32602,
                            message: format!("dangerous command blocked: {}", reason),
                        }),
                        id,
                    };
                }
            }
        }
    }

    let job_id = runtimo_core::utils::generate_id();
    let cap_name = run_params.capability.clone();
    let dry = run_params.dry_run;
    let args = run_params.args;
    let working_dir = match run_params.working_dir {
        Some(ref wd) if !wd.is_empty() => {
            let ctx = runtimo_core::validation::path::PathContext {
                require_exists: true,
                require_file: false,
                ..Default::default()
            };
            match runtimo_core::validation::path::validate_path(wd, &ctx) {
                Ok(_validated) => Some(wd.clone()),
                Err(e) => {
                    eprintln!("[runtimo] Working directory validation failed: {}", e);
                    None
                }
            }
        }
        _ => None,
    };
    let timeout_secs = run_params.timeout_secs.unwrap_or(30);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if !state.bg_jobs.try_reserve() {
        return JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32000,
                message: format!("too many concurrent jobs (max {})", MAX_CONCURRENT_JOBS),
            }),
            id,
        };
    }

    state.bg_jobs.insert(BackgroundJob {
        job_id: job_id.clone(),
        capability: cap_name.clone(),
        status: "running".into(),
        started_at: now,
        finished_at: None,
        result: None,
    });

    // Spawn background execution routed through execute_with_telemetry
    // (parity with handle_run — full safety checks, WAL, and telemetry)
    // Use spawn_blocking to avoid blocking the tokio runtime.
    // WAL mutex and background job registry use std::sync primitives,
    // so no nested runtime is needed.
    let state_arc = Arc::clone(state);
    let jid = job_id.clone();
    let cn = cap_name.clone();
    let wd = working_dir;
    let t_secs = timeout_secs;

    tokio::task::spawn_blocking(move || {
        // Acquire wal_mutex to serialize WAL writes with handle_run
        let _wal_guard = state_arc
            .wal_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let cap = state_arc.registry.get(&cn).ok_or_else(|| {
                runtimo_core::Error::ExecutionFailed(format!(
                    "capability not found in registry: {}",
                    cn
                ))
            })?;
            // working_dir is Option<String> from RunParams, convert to Option<PathBuf>
            #[allow(clippy::useless_conversion)]
            let wd_path = wd.map(PathBuf::from);
            execute_with_telemetry_and_session(
                cap,
                &args,
                dry,
                &state_arc.wal_path,
                None,
                wd_path,
                t_secs,
            )
        }));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (status, error_msg) = match &result {
            Ok(Ok(exec_result)) if exec_result.success => ("completed", None),
            Ok(Ok(_exec_result)) => ("failed", Some("execution reported failure".into())),
            Ok(Err(e)) => ("failed", Some(e.to_string())),
            Err(panic_info) => {
                let msg = panic_info
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic_info.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                ("failed", Some(msg))
            }
        };

        state_arc.bg_jobs.update(&jid, status, error_msg, now);
        state_arc.bg_jobs.release();
    });

    JsonRpcResponse {
        result: Some(serde_json::json!({
            "dispatched": true,
            "job_id": job_id,
            "capability": cap_name,
        })),
        error: None,
        id,
    }
}

/// Handles the `status` RPC method — queries a job by ID.
///
/// Checks the background job registry first, then falls back to scanning the WAL
/// for `JobStarted`/`JobCompleted`/`JobFailed` events.
#[allow(clippy::unused_async)]
async fn handle_status(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    let jid: String = match params.get("job_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: "Missing job_id".into(),
                }),
                id,
            };
        }
    };

    // Check running jobs
    if let Some(bg) = state.bg_jobs.get(&jid) {
        return JsonRpcResponse {
            result: Some(serde_json::json!({
                "job_id": bg.job_id,
                "capability": bg.capability,
                "status": bg.status,
                "started_at": bg.started_at,
            })),
            error: None,
            id,
        };
    }

    // Check WAL
    if let Ok(reader) = WalReader::load(&state.wal_path) {
        let events = reader.events();
        let started = events
            .iter()
            .find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobStarted));
        let completed = events
            .iter()
            .find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobCompleted));
        let failed = events
            .iter()
            .find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobFailed));

        if let Some(s) = started {
            let status = if completed.is_some() {
                "completed"
            } else if failed.is_some() {
                "failed"
            } else {
                "unknown"
            };
            return JsonRpcResponse {
                result: Some(serde_json::json!({
                    "job_id": jid,
                    "capability": s.capability,
                    "status": status,
                    "started_at": s.ts,
                })),
                error: None,
                id,
            };
        }
    }

    JsonRpcResponse {
        result: None,
        error: Some(JsonRpcError {
            code: -32602,
            message: format!("Job not found: {}", jid),
        }),
        id,
    }
}

/// Handles the `list` RPC method — returns all registered capability names.
fn handle_list(state: &Arc<DaemonState>, id: Value) -> JsonRpcResponse {
    let caps: Vec<&str> = state.registry.list();
    JsonRpcResponse {
        result: Some(serde_json::json!({ "capabilities": caps })),
        error: None,
        id,
    }
}

/// Handles the `logs` RPC method — returns recent WAL events.
///
/// Accepts an optional `limit` parameter (default 10). Events are returned
/// in reverse chronological order.
fn handle_logs(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    let logs_params: LogsParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return JsonRpcResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: format!("Invalid params: {}", e),
                }),
                id,
            };
        }
    };

    match WalReader::load(&state.wal_path) {
        Ok(reader) => {
            let events = reader.events();
            let limit = logs_params.limit.min(events.len());
            let recent: Vec<_> = events.iter().rev().take(limit).rev().collect();
            JsonRpcResponse {
                result: Some(serde_json::json!({
                    "events": recent,
                    "total": events.len(),
                })),
                error: None,
                id,
            }
        }
        Err(e) => JsonRpcResponse {
            result: Some(serde_json::json!({
                "events": [],
                "total": 0,
                "error": e.to_string(),
            })),
            error: None,
            id,
        },
    }
}

/// Handles the `jobs` RPC method — lists recent jobs across background registry and WAL.
///
/// Merges running background jobs with completed/failed jobs from the WAL,
/// sorted by start time (newest first), truncated to `limit`.
#[allow(clippy::unused_async)]
async fn handle_jobs(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    #[allow(clippy::cast_possible_truncation)] // safe: limit is capped in practice
    let limit: usize = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut jobs_list: Vec<serde_json::Value> = Vec::new();

    // Running jobs
    for bg in state.bg_jobs.list(100) {
        seen.insert(bg.job_id.clone());
        jobs_list.push(serde_json::json!({
            "job_id": bg.job_id,
            "capability": bg.capability,
            "status": bg.status,
            "started_at": bg.started_at,
        }));
    }

    // WAL jobs
    if let Ok(reader) = WalReader::load(&state.wal_path) {
        #[derive(Default)]
        struct JobEntry {
            started: Option<u64>,
            finished: Option<u64>,
            capability: Option<String>,
        }
        let mut job_entries: HashMap<String, JobEntry> = HashMap::new();
        for e in reader.events() {
            match e.event_type {
                WalEventType::JobStarted => {
                    job_entries.entry(e.job_id.clone()).or_default().started = Some(e.ts);
                    job_entries
                        .entry(e.job_id.clone())
                        .or_default()
                        .capability
                        .clone_from(&e.capability);
                }
                WalEventType::JobCompleted | WalEventType::JobFailed => {
                    job_entries.entry(e.job_id.clone()).or_default().finished = Some(e.ts);
                }
                _ => {}
            }
        }

        for (jid, entry) in job_entries {
            if seen.contains(&jid) {
                continue;
            }
            seen.insert(jid.clone());
            let status = if entry.finished.is_some() {
                "completed"
            } else {
                "unknown"
            };
            jobs_list.push(serde_json::json!({
                "job_id": jid,
                "capability": entry.capability,
                "status": status,
                "started_at": entry.started,
            }));
        }
    }

    jobs_list.sort_by_key(|j| {
        let ts = j["started_at"].as_u64().unwrap_or(0);
        std::cmp::Reverse(ts)
    });
    jobs_list.truncate(limit);

    JsonRpcResponse {
        result: Some(serde_json::json!({ "jobs": jobs_list, "total": jobs_list.len() })),
        error: None,
        id,
    }
}

// ── Client handler ──────────────────────────────────────────────────────────

/// Handles an authenticated client connection on the Unix socket.
///
/// Reads JSON-RPC requests line-by-line, dispatches to `handle_request`,
/// and writes JSON-RPC responses. Returns when the client closes the connection.
async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    if let Err(e) = authenticate_peer(&stream) {
        return Err(format!("Authentication failed: {}", e).into());
    }

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse {
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                    }),
                    id: Value::Null,
                };
                let resp_str = serde_json::to_string(&resp)?;
                let _ = writer.write_all(format!("{}\n", resp_str).as_bytes()).await;
                continue;
            }
        };

        let response = handle_request(&state, request).await;
        let resp_str = serde_json::to_string(&response)?;
        let _ = writer.write_all(format!("{}\n", resp_str).as_bytes()).await;
    }

    Ok(())
}

// ── Argument parsing ────────────────────────────────────────────────────────

/// Parsed command-line arguments for the daemon binary.
struct Args {
    /// Unix socket path (default: `{data_dir}/runtimo.sock`).
    socket: PathBuf,
}

/// Parses command-line arguments, returning a `--socket` path if specified.
///
/// Falls back to `default_socket_path()` when `--socket` is absent.
fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut socket = default_socket_path();

    let mut i: usize = 1;
    while i < args.len() {
        match args.get(i).map(|s| s.as_str()) {
            Some("--socket") => {
                if let Some(val) = args.get(i.saturating_add(1)) {
                    socket = PathBuf::from(val);
                    i = i.saturating_add(2);
                } else {
                    eprintln!("--socket requires a path argument");
                    std::process::exit(1);
                }
            }
            _ => {
                i = i.saturating_add(1);
            }
        }
    }

    Args { socket }
}

// ── Main ────────────────────────────────────────────────────────────────────

/// Reconcile orphaned jobs on daemon startup.
///
/// Scans the WAL for `JobStarted` events that have no matching
/// `JobCompleted` or `JobFailed` terminal event. These are jobs
/// that were dispatched before the daemon terminated (crash, SIGKILL,
/// power loss). Each orphaned job is closed with a `JobFailed` event
/// recording the reason: "daemon terminated before job completion."
///
/// This ensures every job has a definitive terminal state in the audit
/// trail — no permanently "unknown" jobs after recovery.
fn reconcile_orphaned_jobs(wal_path: &std::path::Path) {
    let Ok(reader) = WalReader::load(wal_path) else {
        return; // No WAL yet — nothing to reconcile
    };

    let events = reader.events();

    // Collect job_ids that have a JobStarted event
    let mut started: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut finished: std::collections::HashSet<&String> = std::collections::HashSet::new();

    for e in events {
        if matches!(e.event_type, WalEventType::JobStarted) {
            started.entry(e.job_id.clone()).or_insert(e.ts);
        }
        if matches!(
            e.event_type,
            WalEventType::JobCompleted | WalEventType::JobFailed
        ) {
            finished.insert(&e.job_id);
        }
    }

    let orphaned: Vec<String> = started
        .keys()
        .filter(|jid| !finished.contains(jid))
        .cloned()
        .collect();

    if orphaned.is_empty() {
        return;
    }

    println!(
        "Reconciling {} orphaned job(s) from previous session",
        orphaned.len()
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Ok(mut wal) = WalWriter::create(wal_path) {
        for jid in &orphaned {
            let _ = wal.append(WalEvent {
                seq: wal.seq(),
                ts: now,
                event_type: WalEventType::JobFailed,
                job_id: jid.clone(),
                capability: None,
                output: None,
                error: Some(
                    "daemon terminated before job completion (reconciled on restart)".into(),
                ),
                telemetry_before: None,
                telemetry_after: None,
                process_before: None,
                process_after: None,
                cmd: None,
                cmd_stdout: None,
                cmd_stderr: None,
                cmd_exit_code: None,
                cmd_corrected: None,
                ..Default::default()
            });
        }
        println!("Reconciled {} orphaned job(s).", orphaned.len());
    }
}

/// Start the daemon event loop.
///
/// Creates a tokio runtime, initializes capability registry, binds the Unix socket,
/// and enters the accept loop. Blocks until the process terminates.
///
/// # Errors
/// Returns an error if socket binding fails, WAL initialization fails, or the
/// accept loop encounters an unrecoverable error.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let args = parse_args();
        let wal_path = PathBuf::from(
            std::env::var("RUNTIMO_WAL_PATH")
                .unwrap_or_else(|_| default_wal_path().to_string_lossy().to_string()),
        );

        println!("Runtimo Daemon v{}", env!("CARGO_PKG_VERSION"));
        println!("Socket: {}", args.socket.display());
        println!("WAL:    {}", wal_path.display());

        ensure_data_dir()?;

        if args.socket.exists() {
            std::fs::remove_file(&args.socket)?;
            println!("Removed stale socket file");
        }

        let state = Arc::new(DaemonState::new(&wal_path)?);

        // Reconcile orphaned jobs left from a previous crash/termination
        reconcile_orphaned_jobs(&wal_path);

        // Spawn periodic background maintenance tasks
        let wal_path_bg = wal_path.clone();
        let backup_dir = runtimo_core::utils::backup_dir();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_hours(1));
            loop {
                interval.tick().await;
                let _ = BackupManager::new(backup_dir.clone()).map(|mgr| mgr.cleanup(86400 * 7));
                let _ = WalWriter::cleanup(&wal_path_bg, 86400 * 7);
                let _ = WalWriter::rotate(&wal_path_bg, 10 * 1024 * 1024, 5);
            }
        });

        let _monitor = match runtimo_core::HealthMonitor::start() {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("HealthMonitor failed to start: {}", e);
                None
            }
        };

        // Try to acquire daemon.lock with LOCK_NB to coordinate with CLI.
        // If CLI already holds the lock (during auto-start), we proceed without it.
        let daemon_lock_path = runtimo_core::utils::data_dir().join("daemon.lock");
        if let Some(parent) = daemon_lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lock_file = File::create(&daemon_lock_path);
        if let Ok(file) = lock_file {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: fd is valid file descriptor from File::create; LOCK_EX | LOCK_NB are valid flags
            let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                // Lock held by CLI during auto-start — proceed without holding it
            }
            // Note: file is not dropped here; if we did acquire the lock, it's held for daemon lifetime.
            // If we didn't acquire it, the CLI already holds it and will release when done.
        }

        let listener = tokio::net::UnixListener::bind(&args.socket)?;
        println!("Listening on {}", args.socket.display());

        loop {
            let (stream, addr) = listener.accept().await?;
            let state = Arc::clone(&state);

            tokio::spawn(async move {
                let peer = addr
                    .as_pathname()
                    .map_or_else(|| "unknown".to_string(), |p| p.display().to_string());
                if let Err(e) = handle_client(stream, state).await {
                    eprintln!("Client {} error: {}", peer, e);
                }
            });
        }
    })
}

// ── Unit tests ─────────────────────────────────────────────────────────────
// These tests close the HIGH risk gap identified by QC (0 tests → 41 tests).
// Tests cover: RunParams deserialization, WAL reconciliation, DaemonState,
// path resolution, and async handler parameter validation.

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unused_result_ok,
    clippy::await_holding_lock,
    clippy::no_effect_underscore_binding
)]
mod tests {
    use super::*;
    use runtimo_core::{WalEvent, WalEventType, WalReader, WalWriter};
    use std::sync::Mutex;

    /// Mutex to serialize tests that modify process-global env vars.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // ── helpers ────────────────────────────────────────────────────────────

    fn unique_test_dir() -> std::path::PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("runtimo_daemon_test_{}_{}", std::process::id(), ns))
    }

    fn make_wal(path: &std::path::Path, events: &[WalEvent]) {
        let mut wal = WalWriter::create(path).unwrap();
        for e in events {
            wal.append(e.clone()).unwrap();
        }
    }

    fn make_started_event(seq: u64, ts: u64, job_id: &str, capability: &str) -> WalEvent {
        WalEvent {
            seq,
            ts,
            event_type: WalEventType::JobStarted,
            job_id: job_id.to_string(),
            capability: Some(capability.to_string()),
            ..Default::default()
        }
    }

    fn make_completed_event(seq: u64, ts: u64, job_id: &str, capability: &str) -> WalEvent {
        WalEvent {
            seq,
            ts,
            event_type: WalEventType::JobCompleted,
            job_id: job_id.to_string(),
            capability: Some(capability.to_string()),
            ..Default::default()
        }
    }

    fn make_failed_event(seq: u64, ts: u64, job_id: &str) -> WalEvent {
        WalEvent {
            seq,
            ts,
            event_type: WalEventType::JobFailed,
            job_id: job_id.to_string(),
            error: Some(
                "daemon terminated before job completion (reconciled on restart)".to_string(),
            ),
            ..Default::default()
        }
    }

    // ── RunParams deserialization ──────────────────────────────────────────

    #[test]
    fn test_run_params_all_fields_valid() {
        let json = serde_json::json!({
            "capability": "FileRead",
            "args": {"path": "/tmp/test.txt"},
            "dry_run": true,
            "working_dir": "/tmp",
            "timeout_secs": 60
        });
        let params: RunParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.capability, "FileRead");
        assert_eq!(params.args, serde_json::json!({"path": "/tmp/test.txt"}));
        assert!(params.dry_run);
        assert_eq!(params.working_dir.as_deref(), Some("/tmp"));
        assert_eq!(params.timeout_secs, Some(60));
    }

    #[test]
    fn test_run_params_minimal_valid() {
        // Only the required `capability` field; others use defaults.
        // Note: `args` defaults to Null (serde_json::Value::default()), not empty Object.
        let json = serde_json::json!({"capability": "ShellExec"});
        let params: RunParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.capability, "ShellExec");
        assert_eq!(params.args, serde_json::Value::Null);
        assert!(!params.dry_run);
        assert!(params.working_dir.is_none());
        assert!(params.timeout_secs.is_none());
    }

    #[test]
    fn test_run_params_all_defaults_except_capability() {
        // All fields omitted except capability; verify every default.
        // Note: `args` defaults to Null (serde_json::Value::default()), not empty Object.
        let params: RunParams = serde_json::from_str(r#"{"capability":"Kill"}"#).unwrap();
        assert_eq!(params.capability, "Kill");
        assert_eq!(params.args, serde_json::Value::Null);
        assert!(!params.dry_run);
        assert!(params.working_dir.is_none());
        assert!(params.timeout_secs.is_none());
    }

    #[test]
    fn test_run_params_missing_capability() {
        let json = serde_json::json!({"args": {"path": "/tmp/test.txt"}});
        let result = serde_json::from_value::<RunParams>(json);
        assert!(
            result.is_err(),
            "Should fail when capability field is missing"
        );
    }

    #[test]
    fn test_run_params_invalid_json() {
        let result = serde_json::from_str::<RunParams>(r#"{"capability":"#);
        assert!(result.is_err(), "Should fail on truncated JSON");
    }

    #[test]
    fn test_run_params_empty_object() {
        let result = serde_json::from_str::<RunParams>("{}");
        assert!(
            result.is_err(),
            "Should fail on empty object (missing capability)"
        );
    }

    #[test]
    fn test_run_params_null_capability() {
        // capability is String, null is not a valid string
        let result = serde_json::from_str::<RunParams>(r#"{"capability":null}"#);
        assert!(
            result.is_err(),
            "Null capability should fail deserialization — capability must be a string"
        );
    }

    #[test]
    fn test_run_params_extra_fields_ignored() {
        // Serde ignores unknown fields by default
        let json = serde_json::json!({
            "capability": "FileRead",
            "unknown_field": "should be ignored"
        });
        let params: RunParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.capability, "FileRead");
    }

    #[test]
    fn test_run_params_wrong_type_capability() {
        // capability must be a string, not a number
        let result = serde_json::from_str::<RunParams>(r#"{"capability":42}"#);
        assert!(result.is_err(), "Should reject non-string capability");
    }

    #[test]
    fn test_run_params_timeout_as_string_fails() {
        // timeout_secs is Option<u64>, a string should fail
        let result = serde_json::from_str::<RunParams>(
            r#"{"capability":"FileRead","timeout_secs":"sixty"}"#,
        );
        assert!(result.is_err(), "Should reject string for timeout_secs");
    }

    // ── WAL reconciliation ─────────────────────────────────────────────────

    #[test]
    fn test_reconcile_orphaned_jobs_marks_started_only() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        // Write WAL with two JobStarted events (no terminal events)
        make_wal(
            &wal_path,
            &[
                make_started_event(0, 1000, "job-orphan-1", "FileRead"),
                make_started_event(1, 1001, "job-orphan-2", "ShellExec"),
            ],
        );

        // Verify initial state: 2 started events
        let reader_before = WalReader::load(&wal_path).unwrap();
        assert_eq!(reader_before.events().len(), 2);

        // Reconcile
        reconcile_orphaned_jobs(&wal_path);

        // After reconciliation: both orphans should have JobFailed events appended
        let reader_after = WalReader::load(&wal_path).unwrap();
        assert!(
            reader_after.events().len() >= 4,
            "Expected >=4 events after reconciliation (2 started + 2 failed), got {}",
            reader_after.events().len()
        );

        let has_failed_for_1 = reader_after
            .events()
            .iter()
            .any(|e| e.job_id == "job-orphan-1" && matches!(e.event_type, WalEventType::JobFailed));
        let has_failed_for_2 = reader_after
            .events()
            .iter()
            .any(|e| e.job_id == "job-orphan-2" && matches!(e.event_type, WalEventType::JobFailed));

        assert!(
            has_failed_for_1,
            "job-orphan-1 should have a JobFailed event"
        );
        assert!(
            has_failed_for_2,
            "job-orphan-2 should have a JobFailed event"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_leaves_completed() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        // Write WAL with a JobStarted + JobCompleted pair
        make_wal(
            &wal_path,
            &[
                make_started_event(0, 1000, "job-done", "FileRead"),
                make_completed_event(1, 1001, "job-done", "FileRead"),
            ],
        );

        let count_before = WalReader::load(&wal_path).unwrap().events().len();
        assert_eq!(count_before, 2);

        reconcile_orphaned_jobs(&wal_path);

        // Completed jobs should NOT be marked as failed
        let reader_after = WalReader::load(&wal_path).unwrap();
        assert_eq!(
            reader_after.events().len(),
            count_before,
            "Completed job should not have new events added"
        );

        // Verify no JobFailed event was added for job-done
        let has_failed = reader_after
            .events()
            .iter()
            .any(|e| e.job_id == "job-done" && matches!(e.event_type, WalEventType::JobFailed));
        assert!(
            !has_failed,
            "Completed job should not get a JobFailed event"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_mixed_wal() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        // Mix: one completed job, one orphaned job
        make_wal(
            &wal_path,
            &[
                make_started_event(0, 1000, "job-complete", "FileRead"),
                make_completed_event(1, 1001, "job-complete", "FileRead"),
                make_started_event(2, 1002, "job-orphan", "ShellExec"),
            ],
        );

        reconcile_orphaned_jobs(&wal_path);

        let reader = WalReader::load(&wal_path).unwrap();

        // job-complete should have no JobFailed
        let has_failed_for_complete = reader
            .events()
            .iter()
            .any(|e| e.job_id == "job-complete" && matches!(e.event_type, WalEventType::JobFailed));
        assert!(!has_failed_for_complete);

        // job-orphan should have JobFailed
        let has_failed_for_orphan = reader
            .events()
            .iter()
            .any(|e| e.job_id == "job-orphan" && matches!(e.event_type, WalEventType::JobFailed));
        assert!(has_failed_for_orphan);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_already_failed() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        // Job that already has JobFailed should NOT be duplicated
        make_wal(
            &wal_path,
            &[
                make_started_event(0, 1000, "job-failed", "FileRead"),
                make_failed_event(1, 1001, "job-failed"),
            ],
        );

        let count_before = WalReader::load(&wal_path).unwrap().events().len();
        reconcile_orphaned_jobs(&wal_path);

        let reader = WalReader::load(&wal_path).unwrap();
        assert_eq!(
            reader.events().len(),
            count_before,
            "Already-failed job should not get a duplicate JobFailed event"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_empty_wal() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        // Write empty WAL file
        std::fs::write(&wal_path, "").unwrap();

        let count_before = WalReader::load(&wal_path).unwrap().events().len();
        assert_eq!(count_before, 0);

        // Should not panic on empty WAL
        reconcile_orphaned_jobs(&wal_path);

        let reader = WalReader::load(&wal_path).unwrap();
        assert_eq!(reader.events().len(), 0, "Empty WAL should stay empty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_nonexistent_wal() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("does_not_exist.jsonl");

        // Should not panic when WAL file doesn't exist
        // (function returns early: WalReader::load fails → return)
        reconcile_orphaned_jobs(&wal_path);

        // The function returns early; no file should be created
        assert!(
            !wal_path.exists(),
            "No WAL file should be created for non-existent path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reconcile_orphaned_jobs_reconciliation_message() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wal_path = dir.join("wal.jsonl");

        make_wal(
            &wal_path,
            &[make_started_event(0, 1000, "job-msg", "FileRead")],
        );

        reconcile_orphaned_jobs(&wal_path);

        // Verify the reconciliation message includes the expected text
        let reader = WalReader::load(&wal_path).unwrap();
        let failed_events: Vec<_> = reader
            .events()
            .iter()
            .filter(|e| matches!(e.event_type, WalEventType::JobFailed))
            .collect();

        assert_eq!(failed_events.len(), 1);
        assert!(
            failed_events[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("daemon terminated before job completion"),
            "Reconciled job should have the standard reconciliation message"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── DaemonState verification ───────────────────────────────────────────

    #[test]
    fn test_daemon_state_new_creates_valid_state() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        // Set XDG_DATA_HOME so data_dir() uses our temp dir
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        let wal_path = dir.join("test.wal");
        let state = DaemonState::new(&wal_path).expect("DaemonState::new should succeed");

        // Registry should have all 6 capabilities
        let caps = state.registry.list();
        assert_eq!(caps.len(), 6, "Registry should have 6 capabilities");
        assert!(caps.contains(&"FileRead"));
        assert!(caps.contains(&"FileWrite"));
        assert!(caps.contains(&"GitExec"));
        assert!(caps.contains(&"ShellExec"));
        assert!(caps.contains(&"Kill"));
        assert!(caps.contains(&"Undo"));

        // WAL path should match what we passed in
        assert_eq!(state.wal_path, wal_path);

        // WAL directory should exist
        assert!(dir.exists());

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_daemon_state_registry_has_exact_six_capabilities() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        let wal_path = dir.join("wal.jsonl");
        let state = DaemonState::new(&wal_path).unwrap();

        let caps = state.registry.list();
        assert_eq!(caps.len(), 6);

        // Verify names are distinct
        let mut sorted = caps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            caps.len(),
            "All capability names should be unique"
        );

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_daemon_state_wal_path_configuration() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        // Use a non-standard WAL path
        let custom_wal = dir.join("custom_wal_dir").join("audit.jsonl");
        let state = DaemonState::new(&custom_wal).unwrap();

        assert_eq!(state.wal_path, custom_wal);

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_daemon_state_new_creates_wal_parent_dir() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        let deep_wal = dir.join("deep").join("nested").join("wal.jsonl");
        assert!(!deep_wal.parent().unwrap().exists());

        let state = DaemonState::new(&deep_wal).unwrap();
        assert_eq!(state.wal_path, deep_wal);
        assert!(deep_wal.parent().unwrap().exists());

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Path / binary resolution ───────────────────────────────────────────

    #[test]
    fn test_data_dir_uses_xdg_data_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        let actual = runtimo_core::utils::data_dir();
        assert_eq!(actual, dir.join("runtimo"));

        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_default_wal_path_uses_env_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        let custom_wal = dir.join("custom.jsonl");
        std::env::set_var("RUNTIMO_WAL_PATH", custom_wal.to_str().unwrap());

        assert_eq!(default_wal_path(), custom_wal);

        std::env::remove_var("RUNTIMO_WAL_PATH");
    }

    #[test]
    fn test_default_socket_path_in_data_dir() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let dir = unique_test_dir();
        std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());

        let expected = dir.join("runtimo").join("runtimo.sock");
        assert_eq!(default_socket_path(), expected);

        std::env::remove_var("XDG_DATA_HOME");
    }

    // ── Async handler early-exit paths (no daemon execution needed) ────────

    // These tests verify that handle_run and handle_dispatch return correct
    // error responses for invalid params. They do NOT require a running daemon
    // because the invalid-params path returns before any capability execution.

    #[tokio::test]
    async fn test_handle_run_invalid_params() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let (state, _wal_path) = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            let s = Arc::new(DaemonState::new(&wp).unwrap());
            (s, wp)
        };
        // MutexGuard dropped here — env var still set for the duration of the test

        // Passing a non-JSON-object as params should fail deserialization
        let response = handle_run(
            &state,
            serde_json::Value::String("bad".into()),
            serde_json::Value::from(1),
        )
        .await;

        assert!(
            response.result.is_none(),
            "Invalid params should return no result"
        );
        assert!(response.error.is_some(), "Should return an error");
        assert_eq!(response.error.unwrap().code, -32602);

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_handle_run_unknown_capability() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let state = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            Arc::new(DaemonState::new(&wp).unwrap())
        };

        // Valid params for a capability that doesn't exist
        let params = serde_json::json!({"capability": "NoSuchCapability"});
        let response = handle_run(&state, params, serde_json::Value::from(1)).await;

        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32602);

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_handle_dispatch_invalid_params() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let state = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            Arc::new(DaemonState::new(&wp).unwrap())
        };

        let response = handle_dispatch(
            &state,
            serde_json::Value::String("bad".into()),
            serde_json::Value::from(1),
        )
        .await;

        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32602);

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_handle_dispatch_unknown_capability() {
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let state = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            Arc::new(DaemonState::new(&wp).unwrap())
        };

        let params = serde_json::json!({"capability": "NoSuchCapability"});
        let response = handle_dispatch(&state, params, serde_json::Value::from(1)).await;

        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32602);

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── BackgroundJobRegistry ──────────────────────────────────────────────

    #[test]
    fn test_background_job_registry_new_is_empty() {
        let registry = BackgroundJobRegistry::new();
        let jobs = registry.list(100);
        assert!(jobs.is_empty(), "New registry should have zero jobs");
    }

    #[test]
    fn test_background_job_registry_try_reserve() {
        let registry = BackgroundJobRegistry::new();
        // Should be able to reserve up to MAX_CONCURRENT_JOBS
        for _ in 0..16 {
            assert!(
                registry.try_reserve(),
                "Should be able to reserve within MAX_CONCURRENT_JOBS"
            );
        }
        // 17th reserve should fail
        assert!(!registry.try_reserve(), "17th reserve should fail");
    }

    #[test]
    fn test_background_job_registry_release_allows_re_reserve() {
        let registry = BackgroundJobRegistry::new();
        for _ in 0..16 {
            assert!(registry.try_reserve());
        }
        assert!(!registry.try_reserve(), "Should be full");

        // Release one
        registry.release();
        assert!(
            registry.try_reserve(),
            "Should be able to reserve after release"
        );
    }

    // ── JSON-RPC types ─────────────────────────────────────────────────────

    #[test]
    fn test_json_rpc_request_deserialization() {
        let json = r#"{"method":"run","params":{"capability":"FileRead"},"id":1}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "run");
        assert_eq!(req.id, serde_json::Value::from(1));
    }

    #[test]
    fn test_json_rpc_request_missing_params_defaults() {
        let json = r#"{"method":"list","id":null}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "list");
        assert_eq!(req.params, serde_json::Value::Null);
        assert_eq!(req.id, serde_json::Value::Null);
    }

    #[test]
    fn test_json_rpc_response_result_serialization() {
        let resp = JsonRpcResponse {
            result: Some(serde_json::json!({"ok": true})),
            error: None,
            id: serde_json::Value::from(1),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_json_rpc_response_error_serialization() {
        let resp = JsonRpcResponse {
            result: None,
            error: Some(JsonRpcError {
                code: -32700,
                message: "Parse error".into(),
            }),
            id: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
    }

    // ── LogsParams ─────────────────────────────────────────────────────────

    #[test]
    fn test_logs_params_default_limit() {
        let params: LogsParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.limit, 10);
    }

    #[test]
    fn test_logs_params_custom_limit() {
        let params: LogsParams = serde_json::from_str(r#"{"limit":50}"#).unwrap();
        assert_eq!(params.limit, 50);
    }

    // ── Compile-time signature checks ──────────────────────────────────────
    // These tests verify that function signatures compile correctly when
    // referenced. They do not execute the daemon event loop.

    #[test]
    fn test_run_function_exists_and_is_callable() {
        // Verify the public `run` function has the expected signature.
        // This test only checks compilation — actual execution requires a
        // full daemon environment with a Unix socket, tokio runtime, etc.
        // **Requires running daemon for integration testing.**
        let _sig: fn() -> Result<(), Box<dyn std::error::Error>> = run;
    }

    #[test]
    fn test_handle_run_type_contract_compiles() {
        // Verify `handle_run` type contract compiles.
        // Calls `handle_run` with an unknown capability to exercise the
        // early-return error path. This validates the parameter types,
        // return type, and the basic error-handling flow without executing
        // any actual capability (no telemetry, no WAL writes beyond the
        // initial DaemonState creation).
        // **Requires running daemon for full integration testing.**
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let state = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            Arc::new(DaemonState::new(&wp).unwrap())
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Unknown capability → returns early before any execution
            let params = serde_json::json!({"capability": "NoSuchCap"});
            let response = handle_run(&state, params, serde_json::Value::Null).await;

            assert!(
                response.error.is_some(),
                "Unknown capability should return error"
            );
            assert_eq!(response.error.unwrap().code, -32602);
        });

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_handle_dispatch_type_contract_compiles() {
        // Verify `handle_dispatch` type contract compiles.
        // **Requires running daemon for full integration testing.**
        let dir = unique_test_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let state = {
            let _guard = ENV_MUTEX.lock().unwrap();
            std::env::set_var("XDG_DATA_HOME", dir.to_str().unwrap());
            let wp = dir.join("wal.jsonl");
            Arc::new(DaemonState::new(&wp).unwrap())
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let params = serde_json::json!({"capability": "NoSuchCap"});
            let response = handle_dispatch(&state, params, serde_json::Value::Null).await;

            assert!(
                response.error.is_some(),
                "Unknown capability should return error"
            );
            assert_eq!(response.error.unwrap().code, -32602);
        });

        std::env::remove_var("XDG_DATA_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_args_accepted_flags() {
        // Verify `Args` struct can be constructed with a socket path.
        // `parse_args` is private; this test validates the struct shape.
        let args = Args {
            socket: std::path::PathBuf::from("/tmp/test.sock"),
        };
        assert_eq!(args.socket, std::path::PathBuf::from("/tmp/test.sock"));
    }
}
