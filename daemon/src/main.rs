//! Runtimo Daemon - Unix socket JSON-RPC server for capability execution
//!
//! Usage: runtimo [OPTIONS]
//!
//! Options:
//!   --socket <PATH>    Unix socket path (default: /tmp/runtimo.sock)
//!   --http             Enable HTTP listener (placeholder)
//!   --http-port <PORT> HTTP port (default: 8080)

use runtimo_core::{
    capabilities::{FileRead, FileWrite},
    execute_with_telemetry, CapabilityRegistry, WalReader,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

// Use core's canonical path utilities to avoid drift (G-CTX-1)
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

// ── Request params ──────────────────────────────────────────────────────────

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

// ── Daemon state ────────────────────────────────────────────────────────────

struct DaemonState {
    registry: CapabilityRegistry,
    wal_path: PathBuf,
    wal_mutex: Arc<Mutex<()>>,
}

impl DaemonState {
    fn new(wal_path: &Path) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut registry = CapabilityRegistry::new();
        registry.register(FileRead);

        let backup_dir = data_dir().join("backups");
        let file_write = FileWrite::new(backup_dir)
            .map_err(|e| format!("Failed to create FileWrite capability: {}", e))?;
        registry.register(file_write);

        // Ensure WAL directory exists
        if let Some(parent) = wal_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create WAL directory: {}", e))?;
        }

        Ok(Self {
            registry,
            wal_path: wal_path.to_path_buf(),
            wal_mutex: Arc::new(Mutex::new(())),
        })
    }
}

// ── Request handler ─────────────────────────────────────────────────────────

async fn handle_request(state: &DaemonState, req: JsonRpcRequest) -> JsonRpcResponse {
    match req.method.as_str() {
        "run" => handle_run(state, req.params, req.id).await,
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

    // Acquire WAL mutex to prevent concurrent writes
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
                "telemetry_before": serde_json::to_value(&result.telemetry_before).unwrap_or(Value::Null),
                "telemetry_after": serde_json::to_value(&result.telemetry_after).unwrap_or(Value::Null),
                "process_before": serde_json::to_value(&result.process_before).unwrap_or(Value::Null),
                "process_after": serde_json::to_value(&result.process_after).unwrap_or(Value::Null),
                "wal_seq": result.wal_seq,
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

fn handle_list(state: &DaemonState, id: Value) -> JsonRpcResponse {
    let caps: Vec<&str> = state.registry.list();
    JsonRpcResponse {
        result: Some(serde_json::json!({
            "capabilities": caps,
        })),
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

// ── Client handler ──────────────────────────────────────────────────────────

async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // client disconnected
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

#[allow(dead_code)]
struct Args {
    socket: PathBuf,
    http: bool,
    http_port: u16,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut socket = default_socket_path();
    let mut http = false;
    let mut http_port: u16 = 8080;

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
            "--http" => {
                http = true;
                i += 1;
            }
            "--http-port" => {
                if i + 1 < args.len() {
                    http_port = args[i + 1].parse().unwrap_or(8080);
                    i += 2;
                } else {
                    eprintln!("--http-port requires a port number");
                    std::process::exit(1);
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    Args {
        socket,
        http,
        http_port,
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

    #[tokio::main]
    async fn main() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args();
        let wal_path = PathBuf::from(
            std::env::var("RUNTIMO_WAL_PATH").unwrap_or_else(|_| default_wal_path().to_string_lossy().to_string()),
        );

        println!("Runtimo Daemon v{}", env!("CARGO_PKG_VERSION"));
        println!("Socket: {}", args.socket.display());
        println!("WAL:    {}", wal_path.display());

        // Ensure data directory exists with proper permissions
        ensure_data_dir()?;

        // Clean up stale socket file
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
