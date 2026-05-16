# Changelog

All notable changes to Runtimo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0-alpha] - 2026-05-16

### Added

#### Core Functionality
- **Capability trait** - Pluggable operations with `name()`, `schema()`, `validate()`, `execute()` methods
- **CapabilityRegistry** - Registry for discovering and listing available capabilities
- **Job lifecycle management** - `Job`, `JobId`, `JobState` with state machine transitions
- **Executor pipeline** - `execute_with_telemetry()` wraps capability execution with telemetry and WAL logging
- **JSON Schema validation** - `SchemaValidator` for validating capability arguments against JSON schemas

#### Telemetry (Two-Layer)
- **Hardware telemetry** (`Telemetry::capture()`):
  - CPU model, RAM total/free, disk total/free/used%
  - Uptime, load average
  - TPU/GPU device detection
  - JAX availability check
  - Service detection (vLLM, port 8200)
  - Network info (public IP, cloudflared tunnel status)
- **Process snapshot** (`ProcessSnapshot::capture()`):
  - Full process list via `ps -eo pid,ppid,user,%cpu,%mem,vsz,rss,stat,start,time,comm`
  - Parent PID (PPID) tracking for process lineage
  - Zombie process detection
  - Top CPU/memory consumer identification
  - Summary statistics (total count, total CPU%, total memory%, zombie count)

#### Safety & Security
- **Resource guards** - `LlmoSafeGuard` integration with:
  - Memory ceiling (80% default)
  - CPU load delta measurement
  - Pressure score calculation (0-100%)
  - Raw entropy score (0-1000)
- **Path traversal protection** - Rejects paths containing `..` sequences
- **Empty path validation** - Rejects empty paths
- **Directory read protection** - Rejects attempts to read directories
- **Shell command safety documentation** - Comprehensive security warnings in `cmd.rs`

#### Crash Recovery
- **Write-ahead log (WAL)** - Append-only JSONL log with `fsync` after each write:
  - `JobStarted` events (before validation)
  - `JobCompleted` events (after successful execution)
  - `JobFailed` events (on validation or execution failure)
  - `WalWriter` and `WalReader` for append/read operations
  - Sequential event IDs for ordering guarantees
- **Backup manager** - Automatic backup before file mutation:
  - Pre-mutation file copies stored in `RUNTIMO_BACKUP_DIR/<job_id>/`
  - Undo support via `moe undo -j <job_id>`
  - Parent directory creation for backups

#### Capabilities
- **FileRead** - Read file contents with validation:
  - Schema: `{"path": string}`
  - Validates: path exists, is a file, no traversal
  - Returns: file content as string
- **FileWrite** - Write content to file with backup:
  - Schema: `{"path": string, "content": string, "append": boolean?}`
  - Creates parent directories automatically
  - Creates backup of existing files before overwrite
  - Supports append mode

#### CLI (moe)
- **run** - Execute a capability with args
  - Flags: `-c` (capability), `-a` (args JSON), `--dry-run`
- **list** - List registered capabilities
- **telemetry** - Print hardware/system telemetry
- **processes** - Print process snapshot with PPIDs
- **status** - View job status from WAL
- **logs** - View WAL events (filterable by job ID)
- **undo** - Restore files from backup by job ID

#### Documentation
- Unit tests (13 tests)
- Integration tests (31 tests)
- Doc tests (7 passing, 12 ignored)
- Example programs: `basic_read`, `telemetry_demo`, `write_and_undo`

### Changed

- Renamed from internal project to "Runtimo" (runtime for persistent machines)
- Moved from ad-hoc execution to structured capability pipeline
- Enhanced process snapshot to include PPID (parent PID) tracking
- Added comprehensive security documentation to shell command execution

### Fixed

- **F1: Execution timeout** - Added `execute_with_telemetry_and_timeout()` function with configurable timeout parameter (30s default). Note: timeout parameter accepted but enforcement deferred for future watchdog implementation.
- **F2: PPID tracking** - Changed `ps aux` to `ps -eo pid,ppid,...` format to capture parent PIDs for process lineage tracking
- **F3: Shell command safety** - Added comprehensive security documentation warning against user input interpolation; clarified hardcoded command usage pattern

### Technical Details

#### Workspace Structure
- `runtimo-core` - Core library with capability trait, executor, telemetry, WAL
- `runtimo-cli` (moe) - CLI binary with 7 commands
- `runtimo` (daemon) - Placeholder daemon (future JSON-RPC server)

#### Dependencies
- `serde` + `serde_json` - Serialization
- `thiserror` - Error handling
- `llmosafe` (v0.6) - Resource guard
- `jsonschema` - JSON Schema validation (via examples)

#### Test Coverage
- **13 unit tests** - Capability validation, execution, error handling
- **31 integration tests** - End-to-end workflows, security checks, edge cases
- **7 doc tests** - API documentation examples
- **Total: 51 tests passing**

### Known Issues

#### Not Yet Implemented (v0.1.0)
- Daemon functionality (Unix socket listener, JSON-RPC, job queue)
- Process kill capability for runaway termination
- Backup cleanup (old backups accumulate)
- ShellExec capability
- HTTP request capability
- Concurrent job execution
- WAL path persistence (defaults to /tmp)

### Security Considerations

- All current shell commands in `cmd.rs` are hardcoded literals
- User input must NEVER be interpolated into shell commands
- For user-provided values, use `std::process::Command` directly with proper escaping
- Path traversal (`..`) is rejected but this is not a complete sandbox
- Capabilities run with the same privileges as the host process

### Performance Characteristics

Measured on AMD EPYC 7B13 system:
- Cold start: <1s
- FileRead latency: <10ms for small files
- FileWrite latency: <50ms (includes backup)
- Telemetry capture: <100ms
- Process snapshot: <50ms
- Memory usage: <50MB baseline

### Upgrade Notes

This is the initial release (v0.1.0-alpha). No upgrade path needed.

---

## [Unreleased]

### Planned for v0.2.0
- [ ] Process kill capability
- [ ] ShellExec capability with proper sandboxing
- [ ] HTTP request capability
- [ ] Concurrent job execution with worker pool
- [ ] Daemon JSON-RPC server
- [ ] Backup cleanup policy
- [ ] Configurable WAL path persistence
- [ ] True timeout enforcement (watchdog thread/subprocess)
- [ ] Process lineage tracking (identify spawned PIDs)
- [ ] Alerting on anomalies (zombie threshold, CPU spikes)

### Under Consideration
- Capability versioning
- Capability dependencies
- Job priority queues
- Scheduled execution
- Event filtering/aggregation
- Prometheus metrics export
- Distributed tracing (OpenTelemetry)

---

**Released Versions:**
- v0.1.0-alpha (2026-05-16) - Initial release with FileRead, FileWrite, telemetry, process tracking, WAL, backup/undo
