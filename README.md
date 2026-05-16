# Runtimo

**Agent-centric capability runtime for persistent machines with telemetry, process tracking, and crash recovery.**

[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## What Is Runtimo?

Runtimo is a Rust workspace that provides a **capability execution engine** designed for machines that cannot be factory-reset. Every capability execution is wrapped with:

- **Two-layer telemetry** — Hardware (CPU, RAM, disk, GPU/TPU, services, network) + Process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — `llmosafe` circuit breaker reads `/proc/stat` and `/proc/self/status` to reject execution under pressure
- **Write-ahead log** — Append-only, fsync'd event log for crash recovery
- **Backup/undo** — Files are backed up before mutation, enabling rollback by job ID
- **Hallucination absorption** — Capabilities validate arguments against JSON schemas before execution

**Version:** 0.1.0-alpha (Initial release)  
**License:** MIT  
**Rust Edition:** 2021

## Quick Start

```bash
# Add to your Cargo.toml
[dependencies]
runtimo-core = "0.1"
```

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

// Read a file with full telemetry
let cap = FileRead;
let args = json!({"path": "/etc/hostname"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;

println!("Before: CPU={}, RAM={}", result.telemetry_before.cpu_model, result.telemetry_before.ram_free);
println!("After:  CPU={}, RAM={}", result.telemetry_after.cpu_model, result.telemetry_after.ram_free);
```

## CLI Usage

```bash
# Build from source
cargo build

# List available capabilities
./target/debug/moe list

# Execute a capability with telemetry
./target/debug/moe run -c FileRead -a '{"path":"/etc/hostname"}'

# Write a file (creates automatic backup)
./target/debug/moe run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'

# Dry run (validate without executing)
./target/debug/moe run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run

# View system telemetry
./target/debug/moe telemetry

# View process snapshot (with PPID tracking)
./target/debug/moe processes

# View WAL events
./target/debug/moe logs

# Undo a job (restores from backup)
./target/debug/moe undo -j <job_id>
```

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ moe CLI                                                         │
│ (run, status, undo, logs, telemetry, processes, list)           │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ CapabilityRegistry                                              │
│ ┌──────────┐ ┌──────────┐                                      │
│ │ FileRead │ │FileWrite │                                      │
│ └──────────┘ └──────────┘                                      │
└──────────────────────────┬──────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ execute_with_telemetry()                                        │
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
./target/debug/moe run -c FileRead -a '{"path":"/tmp/test.txt"}'
```

**Security:** Rejects path traversal (`..`), empty paths, directories, and non-existent files.

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
./target/debug/moe run -c FileWrite -a '{"path":"/tmp/out.txt","content":"new content"}'
```

**Example — append:**
```bash
./target/debug/moe run -c FileWrite -a '{"path":"/tmp/out.txt","content":"\nappended line","append":true}'
```

**Security:** Rejects path traversal (`..`) and empty paths. Creates parent directories automatically.

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

## Testing

```bash
# Run all tests
cargo test

# Run core library tests only
cargo test -p runtimo-core

# Run integration tests (31 tests)
cargo test -p runtimo-core --test integration

# Run with output
cargo test -- --nocapture
```

**Test coverage (51 total tests):**

| Category | Tests |
|----------|-------|
| Basic functionality | reads_file_content, writes_file_content, executor_wraps_capability, captures_telemetry, captures_process_snapshot, registry_lists_capabilities |
| Security | rejects_path_traversal_read, rejects_path_traversal_write, rejects_reading_directory, rejects_empty_path |
| Edge cases | reads_empty_file, reads_unicode, reads_large_file, writes_unicode, creates_parent_directories |
| Error handling | rejects_missing_file, rejects_missing_field_in_args, llmosafe_guard_reports_pressure, llmosafe_guard_reports_entropy, llmosafe_guard_check_passes_on_idle_system |
| Workflows | write_then_read_roundtrip, backup_created_on_overwrite, wal_records_jobs, dry_run_does_not_write, append_mode, multiple_jobs_in_sequence |
| Invariants | roundtrip_many_contents, timestamps_monotonic, process_snapshot_consistent, executor_always_returns_telemetry, wal_events_sequential |

## Project Structure

```
runtimo/
├── Cargo.toml              # Workspace definition (v0.1.0, edition 2021)
├── core/                   # runtimo-core library
│   ├── src/
│   │   ├── lib.rs          # Public exports
│   │   ├── capability.rs   # Capability trait + CapabilityRegistry
│   │   ├── executor.rs     # execute_with_telemetry() pipeline
│   │   ├── job.rs          # Job, JobId, JobState lifecycle
│   │   ├── schema.rs       # JSON Schema validator
│   │   ├── telemetry.rs    # Hardware telemetry capture (AMD EPYC, RAM, disk, TPU/GPU, services, network)
│   │   ├── processes.rs    # Process snapshot with PPID tracking
│   │   ├── llmosafe.rs     # llmosafe ResourceGuard integration
│   │   ├── wal.rs          # Write-ahead log (WalWriter, WalReader)
│   │   ├── backup.rs       # BackupManager for undo support
│   │   ├── cmd.rs          # Shell command execution (with security docs)
│   │   └── capabilities/
│   │       ├── mod.rs
│   │       ├── file_read.rs
│   │       └── file_write.rs
│   └── tests/
│       └── integration.rs  # 31 integration tests
├── cli/                    # moe binary
│   └── src/
│       └── main.rs         # 7 CLI commands via clap
└── daemon/                 # runtimo daemon (PLACEHOLDER)
    └── src/
        └── main.rs         # Prints message, sleeps in loop
```

## Known Limitations

### Daemon is Placeholder
The `daemon/` crate (`runtimo`) compiles but only prints a message and sleeps in a loop. Unix socket listener, JSON-RPC protocol, job queue, and HTTP support are **not implemented**. Only the CLI (`moe`) is functional.

### No Process Kill Capability
There is no capability to kill runaway processes. Process tracking is observational only — spawned PIDs are not tracked or terminated.

### WAL Path Defaults to /tmp
The WAL file defaults to `/tmp/runtimo/wal.jsonl` because the daemon may run unprivileged. This means WAL entries are lost on reboot. Set `RUNTIMO_WAL_PATH` for persistence.

### Backup Cleanup is Stub
`BackupManager::cleanup()` is a TODO stub. Old backups accumulate indefinitely.

### No ShellExec or HTTP Capabilities
Only `FileRead` and `FileWrite` are implemented. Shell execution, HTTP requests, and process management capabilities do not exist.

### No Concurrent Job Execution
The executor runs capabilities synchronously. There is no job queue, worker pool, or concurrent execution support.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `/tmp/runtimo/wal.jsonl` | Override WAL file path |
| `RUNTIMO_BACKUP_DIR` | `/tmp/runtimo/backups` | Override backup directory |

## License

MIT License - see [LICENSE](LICENSE) for details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Run `cargo test` to ensure all tests pass
4. Submit a pull request

## Acknowledgments

- Built with hallucination absorption patterns from agent execution research
- Inspired by capability-based security models
- Designed for persistent machines that cannot be factory-reset
