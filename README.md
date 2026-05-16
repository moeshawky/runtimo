# Runtimo

**Agent-centric capability runtime for persistent machines with telemetry, process tracking, and crash recovery.**

[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What Is Runtimo?

Runtimo is a Rust workspace providing a **capability execution engine** designed for machines that cannot be factory-reset. Every capability execution is wrapped with:

- **Two-layer telemetry** — Hardware (CPU, RAM, disk, GPU/TPU, services, network) + Process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — `llmosafe` circuit breaker reads `/proc/stat` and `/proc/self/status` to reject execution under pressure
- **Write-ahead log** — Append-only, fsync'd event log for crash recovery
- **Backup/undo** — Files are backed up before mutation, enabling rollback by job ID
- **Hallucination absorption** — Capabilities validate arguments against JSON schemas before execution

**Version:** 0.1.0  
**License:** MIT  
**Rust Edition:** 2021

## Quick Start

### Library

```bash
cargo add runtimo-core
```

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

// Read a file with full telemetry
let cap = FileRead;
let args = json!({"path": "/tmp/test.txt"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;

println!("Success: {}", result.success);
println!("Job ID: {}", result.job_id);
println!("WAL seq: {}", result.wal_seq);
```

### CLI

```bash
# Build from source
cargo build --release

# List available capabilities
./target/release/moe list

# Execute a capability with telemetry
./target/release/moe run -c FileRead -a '{"path":"/etc/hostname"}'

# Write a file (creates automatic backup)
./target/release/moe run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'

# Dry run (validate without executing)
./target/release/moe run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run

# View system telemetry
./target/release/moe telemetry

# View process snapshot (with PPID tracking)
./target/release/moe processes

# View WAL events
./target/release/moe logs

# Undo a job (restores from backup)
./target/release/moe undo -j <job_id>
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ moe CLI                                                         │
│ (run, status, undo, logs, telemetry, processes, list, session)  │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ CapabilityRegistry                                              │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────┐               │
│ │ FileRead │ │FileWrite │ │ShellExec │ │ Undo │               │
│ └──────────┘ └──────────┘ └──────────┘ └──────┘               │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ execute_with_telemetry() / execute_with_telemetry_and_session() │
│                                                                 │
│ 1. Telemetry::capture()         ← hardware snapshot            │
│ 2. ProcessSnapshot::capture()   ← process list with PPIDs      │
│ 3. LlmoSafeGuard::check()       ← resource guard (80% limit)   │
│ 4. WalWriter::append(Started)   ← WAL event (fsync)            │
│ 5. capability.validate()        ← JSON schema + path checks    │
│ 6. capability.execute()         ← run the capability           │
│ 7. Telemetry::capture()         ← after snapshot               │
│ 8. ProcessSnapshot::capture()   ← after snapshot               │
│ 9. WalWriter::append(Completed) ← WAL event (fsync)            │
│ 10. SessionManager::add_job()   ← track job in session (opt)   │
│                                                                 │
│ Returns: ExecutionResult with before/after telemetry           │
└─────────────────────────────────────────────────────────────────┘
```

## Available Capabilities

### FileRead

Reads the contents of a file. Validates that the path exists, is a file (not a directory), and contains no `..` traversal sequences.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string" }
  },
  "required": ["path"]
}
```

**Example:**
```bash
./target/release/moe run -c FileRead -a '{"path":"/tmp/test.txt"}'
```

**Security:** Rejects path traversal (`..`), empty paths, directories, and non-existent files. File size limited to 100 MB.

### FileWrite

Writes content to a file with automatic backup-before-mutate. If the target file exists, it is copied to the backup directory before being overwritten, enabling undo via `moe undo`.

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

**Example — overwrite:**
```bash
./target/release/moe run -c FileWrite -a '{"path":"/tmp/out.txt","content":"new content"}'
```

**Example — append:**
```bash
./target/release/moe run -c FileWrite -a '{"path":"/tmp/out.txt","content":"\nappended line","append":true}'
```

**Security:** Rejects path traversal (`..`) and empty paths. Creates parent directories automatically. Content size limited to 10 MB.

### ShellExec

Executes shell commands with full telemetry capture, audit logging, and timeout enforcement. Every command is logged to the WAL for audit purposes.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "cmd": { "type": "string" },
    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
    "cwd": { "type": "string" }
  },
  "required": ["cmd"]
}
```

**Example:**
```bash
./target/release/moe run -c ShellExec -a '{"cmd":"uptime"}'
./target/release/moe run -c ShellExec -a '{"cmd":"ls -la /tmp"}'
./target/release/moe run -c ShellExec -a '{"cmd":"pwd","timeout_secs":10}'
```

**Security:** 
- All commands logged to WAL for audit
- Timeout enforcement (default 30s, max 300s)
- Runs with minimal privileges
- **Warning:** Do not interpolate untrusted input into command strings

### Undo

Restores files to their state before a `FileWrite` operation. Uses the WAL to determine original file paths and restores from automatic backups.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "job_id": { "type": "string" }
  },
  "required": ["job_id"]
}
```

**Example:**
```bash
# Write a file (creates backup)
./target/release/moe run -c FileWrite -a '{"path":"/tmp/config.txt","content":"v2"}'
# job: abc123  cap: FileWrite  ok: true

# Undo the write (restores original)
./target/release/moe run -c Undo -a '{"job_id":"abc123"}'
# Or use the dedicated undo command:
./target/release/moe undo -j abc123
```

**How it works:**
1. `FileWrite` backs up the original file to `backups/<job_id>/<filename>`
2. WAL records the original path in the job completion event
3. `Undo` reads the WAL to find the original path, then restores from backup

### Sessions

Sessions group related job executions together, enabling session resume after disconnect, audit trails per session, and batch undo/rollback.

**CLI Commands:**
```bash
# Create a new session
./target/release/moe session --create "ssh-import"
# Created session: abc123  name: ssh-import  status: active

# List all sessions
./target/release/moe session --list
# 2 session(s):
#   abc123 - 5 job(s) [ssh-import]
#   def456 - 3 job(s) [unnamed]

# Run a capability within a session
./target/release/moe run -c FileRead -a '{"path":"/tmp/test.txt"}' --session abc123

# Resume a session (view jobs)
./target/release/moe session --resume abc123
# Session abc123: 5 job(s)
#   - job_id_1
#   - job_id_2
#   ...
```

**Programmatic usage:**
```rust
use runtimo_core::{SessionManager, execute_with_telemetry_and_session};
use std::path::PathBuf;

let mut mgr = SessionManager::new(PathBuf::from("/tmp/sessions")).unwrap();
let session = mgr.create_session(Some("import-job")).unwrap();

// Execute with automatic session tracking
let result = execute_with_telemetry_and_session(
    &cap, &args, false, &wal_path,
    Some(&session.id), 30
)?;
// Job is automatically added to session on completion
```

## Safety Model

### Two-Layer Telemetry (Observational)

Every execution captures hardware and process state before and after:

| Layer | Data Captured |
|-------|---------------|
| **System** | CPU model, RAM total/free, disk total/free/used%, uptime, load average |
| **Hardware** | TPU devices, GPU devices, JAX availability/version/device count |
| **Services** | vLLM version/running status, port 8200 binding |
| **Network** | Public IP, cloudflared tunnel status |
| **Processes** | Full process list with PPIDs, zombie count, top CPU/memory consumers |

Telemetry is **observational** — it records state but does not block execution on its own.

### llmosafe Circuit Breaker (Hard Enforcement)

The `llmosafe::ResourceGuard` is the actual enforcement layer. It reads `/proc/stat` and `/proc/self/status` directly:

- **Memory ceiling**: 80% of system memory by default (configurable)
- **CPU load**: Delta measurement on `/proc/stat`
- **Pressure score**: 0-100% of memory ceiling — execution rejected if > 80%
- **Raw entropy**: 0-1000 weighted score (RSS 50%, IO wait 25%, load 25%)

If the guard rejects, the capability never executes and a `JobFailed` WAL event is recorded.

### WAL Crash Recovery

All events are written to an append-only JSONL file with `fsync` after each write:

| Event Type | When Recorded |
|------------|---------------|
| `JobStarted` | Before validation |
| `JobCompleted` | After successful execution |
| `JobFailed` | On validation or execution failure |
| `JobRolledBack` | On undo (planned) |

### Backup/Undo

`FileWrite` creates a backup copy before mutating any existing file. Backups are stored per-job in `RUNTIMO_BACKUP_DIR/<job_id>/`. The `moe undo` command restores all files from a job's backup directory.

## Performance (Measured)

Measured on AMD EPYC 7B13 system:

| Operation | Latency | Notes |
|-----------|---------|-------|
| Cold start | <1s | Binary load + init |
| FileRead | <10ms | Small files (<1KB) |
| FileWrite | <50ms | Includes backup copy |
| Telemetry capture | <100ms | 15+ shell subprocesses |
| Process snapshot | <50ms | ps aux parse |
| Memory baseline | <50MB | RSS at idle |
| Test suite (59 tests) | <3.5s | Single-threaded |
| Doc build | <4s | No-deps |

## Project Structure

```
runtimo/
├── Cargo.toml              # Workspace definition
├── dist-workspace.toml     # cargo-dist release configuration
├── core/                   # runtimo-core library
│   ├── src/
│   │   ├── lib.rs          # Public exports + error types
│   │   ├── capability.rs   # Capability trait + CapabilityRegistry
│   │   ├── executor.rs     # execute_with_telemetry() pipeline
│   │   ├── job.rs          # Job, JobId, JobState lifecycle
│   │   ├── schema.rs       # JSON Schema validator
│   │   ├── telemetry.rs    # Hardware telemetry capture
│   │   ├── processes.rs    # Process snapshot with PPID tracking
│   │   ├── llmosafe.rs     # llmosafe ResourceGuard integration
│   │   ├── wal.rs          # Write-ahead log (WalWriter, WalReader)
│   │   ├── backup.rs       # BackupManager for undo support
│   │   ├── session.rs      # Session tracking and persistence
│   │   ├── cmd.rs          # Shell command execution
│   │   ├── validation/     # Unified path validation
│   │   │   ├── mod.rs
│   │   │   └── path.rs     # Path traversal + symlink protection
│   │   └── capabilities/
│   │       ├── mod.rs
│   │       ├── file_read.rs
│   │       ├── file_write.rs
│   │       ├── shell_exec.rs
│   │       └── undo.rs
│   └── tests/
│       └── integration.rs  # Integration tests
├── cli/                    # moe binary
│   └── src/
│       └── main.rs         # CLI commands via clap (run, undo, session, etc.)
└── daemon/                 # runtimo-daemon binary
    └── src/
        └── main.rs         # Placeholder (future JSON-RPC server)
```

## Testing

```bash
# Run all tests
cargo test

# Run core library tests only
cargo test -p runtimo-core

# Run with output
cargo test -- --nocapture
```

**Test coverage (59+ total tests):**

| Category | Tests |
|----------|-------|
| Basic functionality | reads_file_content, writes_file_content, executor_wraps_capability, captures_telemetry, captures_process_snapshot, registry_lists_capabilities |
| Security | rejects_path_traversal_read, rejects_path_traversal_write, rejects_reading_directory, rejects_empty_path, rejects_symlink_escape |
| Edge cases | reads_empty_file, reads_unicode, reads_large_file, writes_unicode, creates_parent_directories, truncate_multibyte_utf8 |
| Error handling | rejects_missing_file, rejects_missing_field_in_args, llmosafe_guard_reports_pressure, llmosafe_guard_reports_entropy |
| Workflows | write_then_read_roundtrip, backup_created_on_overwrite, wal_records_jobs, dry_run_does_not_write, append_mode, undo_with_backup |
| Sessions | creates_session, adds_job_to_session, lists_sessions |
| Invariants | roundtrip_many_contents, timestamps_monotonic, process_snapshot_consistent, executor_always_returns_telemetry, wal_events_sequential |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `$XDG_DATA_HOME/runtimo/wal.jsonl` | Override WAL file path |
| `RUNTIMO_BACKUP_DIR` | `$XDG_DATA_HOME/runtimo/backups` | Override backup directory |
| `RUNTIMO_SESSIONS_DIR` | `$XDG_DATA_HOME/runtimo/sessions` | Override session storage directory |

## Known Limitations

### Daemon is Placeholder
The `runtimo-daemon` crate compiles but only prints a message. Unix socket listener, JSON-RPC protocol, job queue, and HTTP support are **not implemented**. Only the CLI (`moe`) is functional.

### No Process Kill Capability
There is no capability to kill runaway processes. Process tracking is observational only — spawned PIDs are not tracked or terminated.

### WAL Path Defaults to /tmp
The WAL file defaults to `$XDG_DATA_HOME/runtimo/wal.jsonl` (falls back to `/tmp`). Set `RUNTIMO_WAL_PATH` for guaranteed persistence.

### Backup Cleanup is Stub
`BackupManager::cleanup()` exists but old backups accumulate indefinitely without a retention policy.

### No HTTP Capability
HTTP requests capability is not yet implemented. FileRead, FileWrite, ShellExec, and Undo are available.

### No Concurrent Job Execution
The executor runs capabilities synchronously. There is no job queue, worker pool, or concurrent execution support.

### Timeout Not Enforced
The `timeout_secs` parameter is accepted for API compatibility but **not currently enforced**. True async timeout requires boxing the capability or using subprocesses. Tracked for v0.2.0.

## License

MIT License - see [LICENSE](LICENSE) for details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Run `cargo test` and `cargo clippy` to ensure all tests pass with no warnings
4. Submit a pull request
