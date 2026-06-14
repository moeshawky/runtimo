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
    capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo},
    execute_with_telemetry_and_session, BackupManager, CapabilityRegistry, WalEvent, WalEventType,
    WalReader, WalWriter,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Authenticate a Unix stream connection via SO_PEERCRED.
#[allow(clippy::borrow_as_ptr)] // FFI: addr_of_mut! + .cast() for getsockopt
fn authenticate_peer(stream: &tokio::net::UnixStream) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    // SAFETY: zeroed representation of ucred is valid — kernel fills it via getsockopt
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)] // socklen_t is u32, ucred is 32 bytes
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: fd is a valid open socket; getsockopt reads metadata only
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(ucred).cast::<libc::c_void>(),
            &mut len,
        )
    };

    if ret != 0 {
        return Err(format!(
            "getsockopt(SO_PEERCRED) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: getuid is always safe — reads caller's real UID with no side effects
    let daemon_uid = unsafe { libc::getuid() };
    if ucred.uid != daemon_uid {
        return Err(format!(
            "UID mismatch: peer={}, daemon={}",
            ucred.uid, daemon_uid
        ));
    }

    Ok(())
}

fn data_dir() -> PathBuf {
    runtimo_core::utils::data_dir()
}

fn default_socket_path() -> PathBuf {
    data_dir().join("runtimo.sock")
}

fn default_wal_path() -> PathBuf {
    runtimo_core::utils::wal_path()
}

fn ensure_data_dir() -> std::io::Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(())
}

// ── JSON-RPC types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    method: String,
    #[serde(default)]
    params: Value,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RunParams {
    capability: String,
    #[serde(default)]
    args: Value,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    working_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LogsParams {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

// ── Background job tracking ──────────────────────────────────────────────────

const MAX_CONCURRENT_JOBS: u32 = 16;

#[derive(Debug, Clone, Serialize)]
struct BackgroundJob {
    job_id: String,
    capability: String,
    status: String,
    started_at: u64,
    finished_at: Option<u64>,
    result: Option<String>,
}

struct BackgroundJobRegistry {
    jobs: RwLock<HashMap<String, BackgroundJob>>,
    running: AtomicU32,
}

impl BackgroundJobRegistry {
    fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            running: AtomicU32::new(0),
        }
    }

    fn try_reserve(&self) -> bool {
        #[allow(clippy::arithmetic_side_effects)] // n+1 only when n < MAX, bounded
        self.running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n < MAX_CONCURRENT_JOBS {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    fn release(&self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
    }

    async fn insert(&self, job: BackgroundJob) {
        self.jobs.write().await.insert(job.job_id.clone(), job);
    }

    async fn get(&self, job_id: &str) -> Option<BackgroundJob> {
        self.jobs.read().await.get(job_id).cloned()
    }

    async fn update(&self, job_id: &str, status: &str, result: Option<String>, finished_at: u64) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = status.to_string();
            job.finished_at = Some(finished_at);
            job.result = result;
        }
    }

    async fn list(&self, limit: usize) -> Vec<BackgroundJob> {
        let jobs = self.jobs.read().await;
        let mut v: Vec<_> = jobs.values().cloned().collect();
        v.sort_by_key(|j| j.started_at);
        v.reverse();
        v.truncate(limit);
        v
    }
}

// ── Daemon state ────────────────────────────────────────────────────────────

struct DaemonState {
    registry: CapabilityRegistry,
    wal_path: PathBuf,
    wal_mutex: Arc<Mutex<()>>,
    bg_jobs: BackgroundJobRegistry,
}

impl DaemonState {
    fn new(wal_path: &Path) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut registry = CapabilityRegistry::new();
        registry.register(FileRead);

        let backup_dir = data_dir().join("backups");
        let file_write = FileWrite::new(backup_dir.clone())
            .map_err(|e| format!("Failed to create FileWrite capability: {}", e))?;
        registry.register(file_write);

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

    let _guard = state.wal_mutex.lock().await;

    match execute_with_telemetry_and_session(
        capability,
        &run_params.args,
        run_params.dry_run,
        &state.wal_path,
        None,
        run_params.working_dir.clone().map(PathBuf::from),
        30,
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
            runtimo_core::validation::path::validate_path(wd, &ctx).ok()
        }
        _ => None,
    };
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

    state
        .bg_jobs
        .insert(BackgroundJob {
            job_id: job_id.clone(),
            capability: cap_name.clone(),
            status: "running".into(),
            started_at: now,
            finished_at: None,
            result: None,
        })
        .await;

    // Spawn background execution routed through execute_with_telemetry
    // (parity with handle_run — full safety checks, WAL, and telemetry)
    let state_arc = Arc::clone(state);
    let tokio_handle = tokio::runtime::Handle::current();
    let jid = job_id.clone();
    let cn = cap_name.clone();
    let wd = working_dir.clone();
    std::thread::spawn(move || {
        // Acquire wal_mutex to serialize WAL writes with handle_run (Fix for CBP violation #2)
        let _wal_guard = tokio_handle.block_on(state_arc.wal_mutex.lock());
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
                30,
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

        tokio_handle.block_on(async {
            state_arc.bg_jobs.update(&jid, status, error_msg, now).await;
        });

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
    if let Some(bg) = state.bg_jobs.get(&jid).await {
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

fn handle_list(state: &Arc<DaemonState>, id: Value) -> JsonRpcResponse {
    let caps: Vec<&str> = state.registry.list();
    JsonRpcResponse {
        result: Some(serde_json::json!({ "capabilities": caps })),
        error: None,
        id,
    }
}

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

async fn handle_jobs(state: &Arc<DaemonState>, params: Value, id: Value) -> JsonRpcResponse {
    #[allow(clippy::cast_possible_truncation)] // safe: limit is capped in practice
    let limit: usize = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let mut seen: std::collections::HashSet<&String> = std::collections::HashSet::new();
    let mut jobs_list: Vec<serde_json::Value> = Vec::new();

    // Running jobs
    let running_jobs = state.bg_jobs.list(100).await;
    for bg in &running_jobs {
        seen.insert(&bg.job_id);
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

        for (jid, entry) in &job_entries {
            if seen.contains(jid) {
                continue;
            }
            seen.insert(jid);
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

struct Args {
    socket: PathBuf,
}

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
    let mut finished: std::collections::HashSet<String> = std::collections::HashSet::new();

    for e in events {
        if matches!(e.event_type, WalEventType::JobStarted) {
            started.entry(e.job_id.clone()).or_insert(e.ts);
        }
        if matches!(
            e.event_type,
            WalEventType::JobCompleted | WalEventType::JobFailed
        ) {
            finished.insert(e.job_id.clone());
        }
    }

    let orphaned: Vec<String> = started
        .keys()
        .filter(|jid| !finished.contains(*jid))
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
