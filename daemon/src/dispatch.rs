//! JSON-RPC types and background job registry for the daemon dispatch system.
//!
//! Provides wire-format types for JSON-RPC request/response handling and the
//! thread-safe background job registry that tracks in-flight and completed
//! capability executions.
//!
//! # Ownership
//! Owns the RPC type definitions and background job lifecycle tracking.
//!
//! # Contracts
//! - `MAX_CONCURRENT_JOBS`: 16 — enforced by `BackgroundJobRegistry::try_reserve()`.
//! - `LogsParams.limit` defaults to 10 via `default_limit()`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

// ── JSON-RPC types ──────────────────────────────────────────────────────────

/// Inbound JSON-RPC request over the Unix socket.
///
/// Parsed from one line of JSON. The `id` field is echoed back in the response
/// for request/response correlation.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// RPC method name (e.g. "run", "dispatch", "status").
    pub method: String,
    /// Method parameters (defaults to JSON null).
    #[serde(default)]
    pub params: Value,
    /// Request identifier for correlation.
    pub id: Value,
}

/// Outbound JSON-RPC response over the Unix socket.
///
/// Contains either a `result` or an `error`, never both.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    /// Successful response data (absent on error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error details (absent on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// Echoed request identifier for correlation.
    pub id: Value,
}

/// JSON-RPC error response body.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    /// Error code (JSON-RPC standard: -32700 parse error, -32601 method not found, etc.).
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
}

/// Parameters for the `run` and `dispatch` JSON-RPC methods.
///
/// Contains the capability name, JSON arguments, dry-run flag, optional
/// working directory, and an optional execution timeout in seconds.
/// Timeout defaults to 30s when not specified.
#[derive(Debug, Deserialize)]
pub struct RunParams {
    /// Capability name to execute (e.g., "FileRead", "ShellExec").
    pub capability: String,
    /// JSON arguments for the capability (defaults to empty object).
    #[serde(default)]
    pub args: Value,
    /// If true, capability skips side effects (dry-run mode).
    #[serde(default)]
    pub dry_run: bool,
    /// Optional working directory for relative path resolution.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional execution timeout in seconds (default: 30).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters for the `logs` JSON-RPC method.
#[derive(Debug, Deserialize)]
pub struct LogsParams {
    /// Maximum number of log entries to return (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Default log entry limit.
fn default_limit() -> usize {
    10
}

// ── Background job tracking ──────────────────────────────────────────────────

/// Maximum concurrent background jobs across all dispatch calls.
pub const MAX_CONCURRENT_JOBS: u32 = 16;

/// A tracked background job dispatched via the `dispatch` RPC method.
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJob {
    /// Unique job identifier.
    pub job_id: String,
    /// Capability name being executed.
    pub capability: String,
    /// Current status: "running", "completed", or "failed".
    pub status: String,
    /// Unix timestamp when the job was dispatched.
    pub started_at: u64,
    /// Unix timestamp when the job finished (absent while running).
    pub finished_at: Option<u64>,
    /// Error message if the job failed.
    pub result: Option<String>,
}

/// Thread-safe registry of in-flight and recently-completed background jobs.
///
/// Uses an atomic counter to enforce `MAX_CONCURRENT_JOBS` and a `std::sync::RwLock`-backed
/// map for status queries. Synchronous methods allow use from both async and
/// blocking (spawn_blocking) contexts without nested runtimes.
pub struct BackgroundJobRegistry {
    /// Job map keyed by job ID.
    jobs: std::sync::RwLock<HashMap<String, BackgroundJob>>,
    /// Count of currently-running background jobs.
    running: AtomicU32,
}

impl BackgroundJobRegistry {
    /// Creates a new empty background job registry.
    pub fn new() -> Self {
        Self {
            jobs: std::sync::RwLock::new(HashMap::new()),
            running: AtomicU32::new(0),
        }
    }

    /// Attempts to reserve a concurrency slot for a new background job.
    ///
    /// Returns `true` if a slot was reserved (current running jobs < MAX_CONCURRENT_JOBS),
    /// `false` if the limit has been reached.
    pub fn try_reserve(&self) -> bool {
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

    /// Releases a concurrency slot after a background job completes.
    pub fn release(&self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
    }

    /// Inserts a new background job into the registry.
    ///
    /// The job is stored with its initial "running" status.
    pub fn insert(&self, job: BackgroundJob) {
        self.jobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job.job_id.clone(), job);
    }

    /// Retrieves a background job by its ID.
    ///
    /// Returns `None` if no job with the given ID exists.
    pub fn get(&self, job_id: &str) -> Option<BackgroundJob> {
        self.jobs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(job_id)
            .cloned()
    }

    /// Updates the status and result of a background job.
    ///
    /// Sets the job's status, finished timestamp, and optional error message.
    /// No-op if the job ID is not found.
    pub fn update(
        &self,
        job_id: &str,
        status: &str,
        result: Option<String>,
        finished_at: u64,
    ) {
        let mut jobs = self.jobs.write().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = status.to_string();
            job.finished_at = Some(finished_at);
            job.result = result;
        }
    }

    /// Lists recent background jobs, newest first.
    ///
    /// Returns up to `limit` jobs sorted by start time (descending).
    pub fn list(&self, limit: usize) -> Vec<BackgroundJob> {
        let jobs = self.jobs.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<_> = jobs.values().cloned().collect();
        v.sort_by_key(|j| j.started_at);
        v.reverse();
        v.truncate(limit);
        v
    }
}
