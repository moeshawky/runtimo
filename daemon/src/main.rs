//! Runtimo Daemon - Unix socket JSON-RPC server for capability execution
//!
//! Usage: runtimo [OPTIONS]
//!
//! Options:
//!   --socket <PATH>    Unix socket path (default: <data>/runtimo.sock)
//!   --http             Enable HTTP listener (placeholder)
//!   --http-port <PORT> HTTP port (default: 8080)
//!
//! # Background Mode
//!
//! Supports `dispatch` — submit a capability, get job ID immediately, check later.
//! Uses `status` and `jobs` RPC methods for queriable job history.

use runtimo_core::{
    capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo},
    execute_with_telemetry, CapabilityRegistry, WalReader, WalWriter, WalEvent, WalEventType,
    Context,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Authenticate a Unix stream connection via SO_PEERCRED.
fn authenticate_peer(stream: &tokio::net::UnixStream) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut _,
            &mut len,
        )
    };

    if ret != 0 {
        return Err(format!(
            "getsockopt(SO_PEERCRED) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

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
}

impl BackgroundJobRegistry {
    fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
        }
    }

    async fn insert(&self, job: BackgroundJob) {
        self.jobs.write().await.insert(job.job_id.clone(), job);
    }

    async fn get(&self, job_id: &str) -> Option<BackgroundJob> {
        self.jobs.read().await.get(job_id).cloned()
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

        let git_exec = GitExec::new(backup_dir.clone())
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

async fn handle_request(state: &DaemonState, req: JsonRpcRequest) -> JsonRpcResponse {
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

async fn handle_run(state: &DaemonState, params: Value, id: Value) -> JsonRpcResponse {
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

    let capability = match state.registry.get(&run_params.capability) {
        Some(c) => c,
        None => {
            return JsonRpcResponse {
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: format!("Capability not found: {}", run_params.capability),
                }),
                id,
            };
        }
    };

    let _guard = state.wal_mutex.lock().await;

    match execute_with_telemetry(
        capability,
        &run_params.args,
        run_params.dry_run,
        &state.wal_path,
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

async fn handle_dispatch(state: &DaemonState, params: Value, id: Value) -> JsonRpcResponse {
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    state.bg_jobs.insert(BackgroundJob {
        job_id: job_id.clone(),
        capability: cap_name.clone(),
        status: "running".into(),
        started_at: now,
        finished_at: None,
        result: None,
    }).await;

    // Log JobStarted to WAL
    {
        let _guard = state.wal_mutex.lock().await;
        if let Ok(mut wal) = WalWriter::create(&state.wal_path) {
            let _ = wal.append(WalEvent {
                seq: wal.seq(),
                ts: now,
                event_type: WalEventType::JobStarted,
                job_id: job_id.clone(),
                capability: Some(cap_name.clone()),
                output: None,
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
            });
        }
    }

    // Spawn background execution in a thread
    let _wal_clone = state.wal_path.clone();
    let jid = job_id.clone();
    let cn = cap_name.clone();
    std::thread::spawn(move || {
        let backup_dir = runtimo_core::utils::backup_dir();
        let mut registry = CapabilityRegistry::new();
        registry.register(FileRead);
        if let Ok(fw) = FileWrite::new(backup_dir.clone()) { registry.register(fw); }
        if let Ok(ge) = GitExec::new(backup_dir.clone()) { registry.register(ge); }
        registry.register(ShellExec);
        registry.register(Kill);
        registry.register(Undo);

        if let Some(cap) = registry.get(&cn) {
            let ctx = Context {
                dry_run: dry,
                job_id: jid,
                working_dir: std::env::current_dir().unwrap_or_default(),
            };
            let _ = cap.execute(&args, &ctx);
        }
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

async fn handle_status(state: &DaemonState, params: Value, id: Value) -> JsonRpcResponse {
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
        let started = events.iter().find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobStarted));
        let completed = events.iter().find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobCompleted));
        let failed = events.iter().find(|e| e.job_id == jid && matches!(e.event_type, WalEventType::JobFailed));

        if let Some(s) = started {
            let status = if completed.is_some() { "completed" } else if failed.is_some() { "failed" } else { "unknown" };
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

fn handle_list(state: &DaemonState, id: Value) -> JsonRpcResponse {
    let caps: Vec<&str> = state.registry.list();
    JsonRpcResponse {
        result: Some(serde_json::json!({ "capabilities": caps })),
        error: None,
        id,
    }
}

fn handle_logs(state: &DaemonState, params: Value, id: Value) -> JsonRpcResponse {
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

async fn handle_jobs(state: &DaemonState, params: Value, id: Value) -> JsonRpcResponse {
    let limit: usize = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut jobs_list: Vec<serde_json::Value> = Vec::new();

    // Running jobs
    for bg in state.bg_jobs.list(100).await {
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
                    job_entries.entry(e.job_id.clone()).or_default().capability = e.capability.clone();
                }
                WalEventType::JobCompleted | WalEventType::JobFailed => {
                    job_entries.entry(e.job_id.clone()).or_default().finished = Some(e.ts);
                }
                _ => {}
            }
        }

        for (jid, entry) in job_entries {
            if seen.contains(&jid) { continue; }
            seen.insert(jid.clone());
            let status = if entry.finished.is_some() { "completed" } else { "unknown" };
            jobs_list.push(serde_json::json!({
                "job_id": jid,
                "capability": entry.capability,
                "status": status,
                "started_at": entry.started,
            }));
        }
    }

    jobs_list.sort_by_key(|j| -(j["started_at"].as_u64().unwrap_or(0) as i64));
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

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                if i + 1 < args.len() {
                    socket = PathBuf::from(&args[i + 1]);
                    i += 2;
                } else {
                    eprintln!("--socket requires a path argument");
                    std::process::exit(1);
                }
            }
            _ => { i += 1; }
        }
    }

    Args { socket }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let listener = tokio::net::UnixListener::bind(&args.socket)?;
    println!("Listening on {}", args.socket.display());

    loop {
        let (stream, addr) = listener.accept().await?;
        let state = Arc::clone(&state);

        tokio::spawn(async move {
            let peer = addr
                .as_pathname()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            if let Err(e) = handle_client(stream, state).await {
                eprintln!("Client {} error: {}", peer, e);
            }
        });
    }
}
