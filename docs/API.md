# Runtimo Core API Reference

**Version:** 0.7.1
**Updated:** 2026-05-28
**Documentation:** [docs.rs/runtimo-core](https://docs.rs/runtimo-core/0.7.1)

## Quick Links

- [Getting Started](GETTING_STARTED.md)
- [Architecture](ARCHITECTURE.md)
- [Runbooks](runbooks/)
- [Changelog](../CHANGELOG.md)

## Core Types

### Capability Trait

The foundation of Runtimo is the [`Capability`](../core/src/lib.rs) trait:

```rust
pub trait Capability {
    /// Unique identifier for this capability
    fn name(&self) -> &'static str;
    
    /// JSON Schema for argument validation
    fn schema(&self) -> &'static str;
    
    /// Validate arguments against schema
    fn validate(&self, args: &Value) -> Result<()>;
    
    /// Execute the capability
    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output>;
}
```

### Context

Execution context passed to capabilities:

```rust
pub struct Context {
    pub dry_run: bool,
    pub job_id: String,
    pub working_dir: PathBuf,
}
```

### Output

Capability execution result:

```rust
pub struct Output {
    pub success: bool,
    pub data: Value,
    pub message: Option<String>,
}
```

## Built-in Capabilities

### FileRead

**Purpose:** Read file contents with validation, binary detection, JSON auto-parse

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "max_bytes": { "type": "integer" }
  },
  "required": ["path"]
}
```

**Security:**
- Rejects paths containing `..`
- Rejects empty paths
- Rejects directories
- Rejects non-existent files
- O_NOFOLLOW on open (prevents TOCTOU symlink escape)
- Binary content detection (null bytes → `content_type: "binary"`)
- Bounded reader (max 100 MB)
- UTF-8 safe truncation on multibyte boundaries

**Example:**
```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileRead;
let args = json!({"path": "/etc/hostname"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
println!("Content: {}", result.output.data["content"]);
```

### FileWrite

**Purpose:** Write content to file with automatic backup

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" },
    "content": { "type": "string" },
    "append": { "type": "boolean" }
  },
  "required": ["path", "content"]
}
```

**Features:**
- Creates parent directories automatically
- Backs up existing files before overwrite
- Supports append mode
- Undo support via `BackupManager`

**Example:**
```rust
use runtimo_core::{FileWrite, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileWrite;
let args = json!({
    "path": "/tmp/hello.txt",
    "content": "hello runtimo"
});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
```

## Telemetry

### Hardware Telemetry

```rust
pub struct Telemetry {
    pub timestamp: i64,
    pub cpu_model: String,
    pub ram_total_gb: f64,
    pub ram_free_gb: f64,
    pub disk_total_gb: f64,
    pub disk_free_gb: f64,
    pub disk_used_percent: f64,
    pub uptime_secs: i64,
    pub load_avg: [f64; 3],
    pub tpu_count: u32,
    pub gpu_count: u32,
    pub jax_available: bool,
    pub vllm_version: Option<String>,
    pub vllm_running: bool,
    pub port_8200_bound: bool,
    pub public_ip: Option<String>,
    pub tunnel_running: bool,
}
```

**Usage:**
```rust
use runtimo_core::Telemetry;

let telemetry = Telemetry::capture();
println!("CPU: {}", telemetry.cpu_model);
println!("RAM: {}GB free", telemetry.ram_free_gb);
println!("Disk: {}% used", telemetry.disk_used_percent);
```

### Process Snapshot

```rust
pub struct ProcessSnapshot {
    pub processes: Vec<ProcessInfo>,
    pub summary: ProcessSummary,
}

pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,  // Parent PID
    pub user: String,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub vsz: u64,
    pub rss: u64,
    pub stat: String,
    pub start: String,
    pub time: String,
    pub comm: String,
}

pub struct ProcessSummary {
    pub total_processes: u32,
    pub total_cpu_percent: f64,
    pub total_mem_percent: f64,
    pub zombie_count: u32,
    pub top_cpu_process: String,
    pub top_mem_process: String,
}
```

**Usage:**
```rust
use runtimo_core::ProcessSnapshot;

let snapshot = ProcessSnapshot::capture();
println!("Total processes: {}", snapshot.summary.total_processes);
println!("Zombies: {}", snapshot.summary.zombie_count);
println!("Top CPU: {}", snapshot.summary.top_cpu_process);
```

## Resource Guards

### LlmoSafeGuard

Circuit breaker that reads `/proc/stat` and `/proc/self/status`:

```rust
use runtimo_core::LlmoSafeGuard;

let guard = LlmoSafeGuard::new();
guard.check()?;  // Returns Err if resources exceeded
```

**Thresholds:**
- Memory: 80% of system total
- CPU: Delta measurement on `/proc/stat`
- Pressure score: 0-100% (reject if > 80%)
- Raw entropy: 0-1000 (RSS 50%, IO wait 25%, load 25%)

## Write-Ahead Log

### WalWriter

Append-only log with fsync, file locking, rotation, and cleanup:

```rust
use runtimo_core::{WalWriter, WalEvent, WalEventType};
use std::path::Path;

let mut wal = WalWriter::create(Path::new("/tmp/wal.jsonl"))?;

wal.append(WalEvent {
    seq: 0,
    ts: 1234567890,
    event_type: WalEventType::JobStarted,
    job_id: "job-123".to_string(),
    capability: Some("FileRead".to_string()),
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
})?;
```

### WalEventType

```rust
pub enum WalEventType {
    JobSubmitted,    // Job submitted to system
    JobValidated,    // Args passed validation
    JobStarted,      // Execution started
    JobCompleted,    // Execution succeeded
    JobFailed,       // Validation or execution failure
    JobRolledBack,   // Job rolled back via undo
    CommandExecuted, // Shell command recorded (dev-only, debug builds)
}
```

### CommandExecuted Events (Dev-Only)

When `event_type` is `CommandExecuted`, the following fields capture shell command details:

| Field | Type | Description |
|-------|------|-------------|
| `cmd` | `Option<String>` | Shell command string |
| `cmd_stdout` | `Option<String>` | Captured stdout (truncated to 1KB) |
| `cmd_stderr` | `Option<String>` | Captured stderr (truncated to 1KB) |
| `cmd_exit_code` | `Option<i32>` | Command exit code |
| `cmd_corrected` | `Option<String>` | Auto-corrected command (Phase 2) |

**Note:** CommandExecuted events are only written in debug builds (`#[cfg(debug_assertions)]`). The variant exists in release builds for reading old WALs but is never produced.

### WalReader

Read WAL events:

```rust
use runtimo_core::WalReader;
use std::path::Path;

let reader = WalReader::open(Path::new("/tmp/wal.jsonl"))?;

for event in reader.iter() {
    println!("Event: {:?}", event?);
}
```

## Backup Manager

### BackupManager

Pre-mutation backups with undo support:

```rust
use runtimo_core::BackupManager;
use std::path::Path;

let backup_mgr = BackupManager::new(Path::new("/tmp/backups"));

// Create backup before mutation
let backup_path = backup_mgr.backup("/tmp/important.txt", "job-123")?;

// Later, restore from backup
backup_mgr.restore("/tmp/important.txt", "job-123")?;
```

## Execution Pipeline

### execute_with_telemetry

Main execution function:

```rust
pub fn execute_with_telemetry(
    capability: &dyn Capability,
    args: &Value,
    dry_run: bool,
    wal_path: &Path,
) -> Result<ExecutionResult>
```

**Pipeline:**
1. Capture hardware telemetry (before)
2. Capture process snapshot (before)
3. Check resource guard
4. Log `JobStarted` to WAL
5. Validate arguments against schema
6. Execute capability
7. Capture hardware telemetry (after)
8. Capture process snapshot (after)
9. Log `JobCompleted` or `JobFailed` to WAL
10. (Dev-only) Log `CommandExecuted` to WAL for ShellExec

### ExecutionResult

```rust
pub struct ExecutionResult {
    pub job_id: String,
    pub capability: String,
    pub success: bool,
    pub output: Output,
    pub telemetry_before: Telemetry,
    pub telemetry_after: Telemetry,
    pub process_before: ProcessSummary,
    pub process_after: ProcessSummary,
    pub wal_seq: u64,
}
```

## Job Management

### Job

Lifecycle-tracked execution unit:

```rust
use runtimo_core::{Job, JobState};

let mut job = Job::new("FileRead");
assert_eq!(job.state(), &JobState::Created);

job.transition(JobState::Running)?;
assert_eq!(job.state(), &JobState::Running);

job.transition(JobState::Completed)?;
assert_eq!(job.state(), &JobState::Completed);
```

### JobState

```rust
pub enum JobState {
    Created,
    Running,
    Completed,
    Failed,
    RolledBack,
}
```

## Error Handling

### Error Enum

```rust
pub enum Error {
    InvalidTransition { from: JobState, to: JobState },
    SchemaValidationFailed(String),
    CapabilityNotFound(String),
    ExecutionFailed(String),
    WalError(String),
    BackupError(String),
    ResourceLimitExceeded(String),
    TelemetryError(String),
}
```

### Pattern Matching

```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry, Error};
use serde_json::json;

match execute_with_telemetry(&FileRead, &json!({"path": "/test"}), false, wal_path) {
    Ok(result) => {
        println!("Success: {:?}", result.output);
    }
    Err(Error::SchemaValidationFailed(msg)) => {
        eprintln!("Validation error: {}", msg);
    }
    Err(Error::ExecutionFailed(msg)) => {
        eprintln!("Execution error: {}", msg);
    }
    Err(Error::ResourceLimitExceeded(msg)) => {
        eprintln!("Resource error: {}", msg);
    }
    Err(e) => {
        eprintln!("Unexpected error: {}", e);
    }
}
```

## CLI Commands

### runtimo run

Execute a capability:

```bash
runtimo run -c FileRead -a '{"path":"/etc/hostname"}'
runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run
```

### runtimo list

List registered capabilities:

```bash
runtimo list
```

### runtimo telemetry

View hardware telemetry:

```bash
runtimo telemetry
```

### runtimo processes

View process snapshot:

```bash
runtimo processes
```

### runtimo status

View job status:

```bash
runtimo status
runtimo status -j <job_id>
```

### runtimo logs

View WAL events:

```bash
runtimo logs
runtimo logs -j <job_id> -l 20
```

### runtimo undo

Restore from backup:

```bash
runtimo undo -j <job_id>
```

### runtimo config

Manage allowed path prefixes:

```bash
runtimo config allowed-paths add /srv
runtimo config allowed-paths list
runtimo config allowed-paths remove /srv
```

### runtimo session

Create, list, and resume sessions:

```bash
runtimo session --create "my-task"
runtimo session --list
runtimo session --resume <session_id>
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `/tmp/runtimo/wal.jsonl` | WAL file path |

## Examples

See `core/examples/` for complete examples:
- `basic_read.rs` - Basic file read
- `telemetry_demo.rs` - Telemetry demonstration
- `write_and_undo.rs` - Write and undo pattern

## Testing

```bash
cargo test -p runtimo-core
cargo test -p runtimo-core --test integration
cargo test -- --nocapture
```

## Version

0.7.1

## License

MIT License
