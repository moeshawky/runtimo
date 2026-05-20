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

**Version:** 0.2.1
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
./target/release/runtimo list

# Execute a capability with telemetry
./target/release/runtimo run -c FileRead -a '{"path":"/etc/hostname"}'

# Write a file (creates automatic backup)
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'

# Dry run (validate without executing)
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run

# View system telemetry
./target/release/runtimo telemetry

# View process snapshot (with PPID tracking)
./target/release/runtimo processes

# View WAL events
./target/release/runtimo logs

# Undo a job (restores from backup)
./target/release/runtimo undo -j <job_id>
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ runtimo CLI │
│ (run, status, undo, logs, telemetry, processes, list, session) │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ CapabilityRegistry                                              │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────┐ ┌──────┐ ┌─────────┐ │
│ │ FileRead │ │FileWrite │ │ShellExec │ │ Undo │ │ Kill │ │ GitExec │ │
│ └──────────┘ └──────────┘ └──────────┘ └──────┘ └──────┘ └─────────┘ │
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
./target/release/runtimo run -c FileRead -a '{"path":"/tmp/test.txt"}'
```

**Security:** Rejects path traversal (`..`), empty paths, directories, and non-existent files. File size limited to 100 MB.

### FileWrite

Writes content to a file with automatic backup-before-mutate. If the target file exists, it is copied to the backup directory before being overwritten, enabling undo via `runtimo undo`.

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
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/out.txt","content":"new content"}'
```

**Example — append:**
```bash
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/out.txt","content":"\nappended line","append":true}'
```

**Security:** Rejects path traversal (`..`) and empty paths. Creates parent directories automatically. Content size limited to 10 MB.

### ShellExec

Executes shell commands via `sh -c` with full telemetry capture, audit logging, timeout enforcement, and dangerous command blocklist. Every command is logged to the WAL for audit purposes.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "cmd": { "type": "string" },
    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
    "cwd": { "type": "string" },
    "stdin": { "type": "string" }
  },
  "required": ["cmd"]
}
```

**Example:**
```bash
runtimo run -c ShellExec -a '{"cmd":"uptime"}'
runtimo run -c ShellExec -a '{"cmd":"ls -la /tmp"}'
runtimo run -c ShellExec -a '{"cmd":"pwd","cwd":"/tmp"}'
# Pipes and chaining work
runtimo run -c ShellExec -a '{"cmd":"ls | head -3"}'
runtimo run -c ShellExec -a '{"cmd":"echo hi && whoami"}'
```

**Security & Guardrails (agent mistake protection, not security):**
- **Always `sh -c`** — supports pipes, redirects, chaining, variables
- **Timeout enforcement** — default 30s, max 300s, kills all descendants
- **Dangerous command blocklist:** `mkfs.*`, `fdisk`, `parted`, `dd`, `shutdown`, `reboot`, `poweroff`, `rm -rf /` (on root/dev/boot), `chmod 777 /`
- **Process group isolation** — kills entire process tree on timeout
- **WAL audit trail** — every command logged (dev-only: stdout/stderr/exit code)
- **Stdin pipe support** — for piping data into commands

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
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/config.txt","content":"v2"}'
# job: abc123  cap: FileWrite  ok: true

# Undo the write (restores original)
./target/release/runtimo run -c Undo -a '{"job_id":"abc123"}'
# Or use the dedicated undo command:
./target/release/runtimo undo -j abc123
```

**How it works:**
1. `FileWrite` backs up the original file to `backups/<job_id>/<filename>`
2. WAL records the original path in the job completion event
3. `Undo` reads the WAL to find the original path, then restores from backup

### Kill

Terminates a process by PID with full audit logging. Includes safety guards to prevent killing critical system processes.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "pid": { "type": "integer", "minimum": 1 },
    "signal": { "type": "integer", "minimum": -64, "maximum": 64 }
  },
  "required": ["pid"]
}
```

**Example:**
```bash
./target/release/runtimo run -c Kill -a '{"pid":12345}'
./target/release/runtimo run -c Kill -a '{"pid":12345,"signal":9}'
```

**Security:** Protected PIDs include init (1), kthreadd (2), the daemon's own PID, and its parent PID. These cannot be killed.

### GitExec

Executes git operations (clone, pull, commit, revert, clean, status) with state tracking, backup-before-mutate, and WAL logging.

**Schema:**
```json
{
  "type": "object",
  "properties": {
    "operation": { "type": "string", "enum": ["clone", "pull", "commit", "revert", "clean", "status"] },
    "url": { "type": "string" },
    "path": { "type": "string" },
    "branch": { "type": "string" },
    "message": { "type": "string" },
    "files": { "type": "array", "items": { "type": "string" } },
    "commit_sha": { "type": "string" },
    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
  },
  "required": ["operation"]
}
```

**Example:**
```bash
./target/release/runtimo run -c GitExec -a '{"operation":"status","path":"/tmp/myrepo"}'
./target/release/runtimo run -c GitExec -a '{"operation":"clone","url":"https://github.com/user/repo.git","path":"/tmp/repo"}'
```

**Security:** URL validation (http/https/SSH), path traversal protection, branch name and commit SHA validation.

### Sessions

Sessions group related job executions together, enabling session resume after disconnect, audit trails per session, and batch undo/rollback.

**CLI Commands:**
```bash
# Create a new session
./target/release/runtimo session --create "ssh-import"
# Created session: abc123  name: ssh-import  status: active

# List all sessions
./target/release/runtimo session --list
# 2 session(s):
#   abc123 - 5 job(s) [ssh-import]
#   def456 - 3 job(s) [unnamed]

# Run a capability within a session
./target/release/runtimo run -c FileRead -a '{"path":"/tmp/test.txt"}' --session abc123

# Resume a session (view jobs)
./target/release/runtimo session --resume abc123
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
| `CommandExecuted` | (Dev-only) Shell command executed — records cmd, stdout/stderr (1KB trunc), exit code, correction |

**Dev-only note:** `CommandExecuted` events are only written in debug builds (`#[cfg(debug_assertions)]`). The variant exists in release for reading old WALs. This prevents WAL bloat in production while enabling error-pattern analysis during development.

### Backup/Undo

`FileWrite` creates a backup copy before mutating any existing file. Backups are stored per-job in `RUNTIMO_BACKUP_DIR/<job_id>/`. The `runtimo undo` command restores all files from a job's backup directory.

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
| Test suite (176 tests) | <4s | Single-threaded |
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
│   │       ├── kill.rs
│   │       ├── git_exec.rs
│   │       └── undo.rs
│   └── tests/
│       └── integration.rs  # Integration tests
├── cli/ # runtimo binary
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

**Test coverage (176 total tests):**

| Category | Tests |
|----------|-------|
| Basic functionality | reads_file_content, writes_file_content, executor_wraps_capability, captures_telemetry, captures_process_snapshot, registry_lists_capabilities |
| Security | rejects_path_traversal_read, rejects_path_traversal_write, rejects_reading_directory, rejects_empty_path, rejects_symlink_escape |
| Edge cases | reads_empty_file, reads_unicode, reads_large_file, writes_unicode, creates_parent_directories, truncate_multibyte_utf8 |
| Error handling | rejects_missing_file, rejects_missing_field_in_args, llmosafe_guard_reports_pressure, llmosafe_guard_reports_entropy |
| Workflows | write_then_read_roundtrip, backup_created_on_overwrite, wal_records_jobs, dry_run_does_not_write, append_mode, undo_with_backup |
| Sessions | creates_session, adds_job_to_session, lists_sessions |
| Invariants | roundtrip_many_contents, timestamps_monotonic, process_snapshot_consistent, executor_always_returns_telemetry, wal_events_sequential |
| Kill capability | test_kill_schema, test_kill_protected_pid, test_kill_self_protected, test_kill_nonexistent, test_kill_actual_process |
| HealthMonitor | test_health_monitor_lifecycle, test_health_state_defaults, test_cpu_alert_after_consecutive_checks, test_ram_alert_uses_ram_counter_not_cpu, test_ram_alert_resets_when_ram_decreases, test_parse_size_value |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `$XDG_DATA_HOME/runtimo/wal.jsonl` | Override WAL file path |
| `RUNTIMO_BACKUP_DIR` | `$XDG_DATA_HOME/runtimo/backups` | Override backup directory |
| `RUNTIMO_SESSIONS_DIR` | `$XDG_DATA_HOME/runtimo/sessions` | Override session storage directory |

## Known Limitations

### Daemon Authentication
The daemon uses `SO_PEERCRED` UID matching for authentication. Only processes running as the same user can connect. For multi-user environments, consider adding group-based access control or TLS.

### WAL Path Defaults to XDG Data Home
The WAL file defaults to `$XDG_DATA_HOME/runtimo/wal.jsonl`. Set `RUNTIMO_WAL_PATH` for explicit control.

### Backup Cleanup is Stub
`BackupManager::cleanup()` exists but old backups accumulate indefinitely without a retention policy.

### No HTTP Capability
HTTP requests capability is not yet implemented. FileRead, FileWrite, ShellExec, Undo, Kill, and GitExec are available.

### No Concurrent Job Execution
The executor runs capabilities synchronously. There is no job queue, worker pool, or concurrent execution support.

### Timeout Enforcement is Post-Execution
The `timeout_secs` parameter is measured after capability execution completes. If a capability blocks indefinitely (e.g., waiting on I/O), the timeout is detected after the fact. True pre-emptive timeout requires subprocess isolation.

## License

MIT License - see [LICENSE](LICENSE) for details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Run `cargo test` and `cargo clippy` to ensure all tests pass with no warnings
4. Submit a pull request
