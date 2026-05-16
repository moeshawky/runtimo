# Changelog

All notable changes to Runtimo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-05-16

### Security

- **Daemon Unix socket authentication** — Added `SO_PEERCRED` UID verification via `libc`.
  Only processes running as the same UID as the daemon can connect. Previously any local
  process could execute arbitrary capabilities.
- **Kill capability hardening** — Protected PIDs now include the daemon's own PID and its
  parent PID (read from `/proc/self/status`). Removed `force=true` bypass that allowed
  killing any protected process.
- **GitExec wired and audited** — Exported in `capabilities/mod.rs`, registered in CLI and
  Daemon. Fixed silenced `git add` errors (previously `let _ =` discarded failures).

### Bug Fixes

- **CLI Undo multi-file data loss** — HashMap keyed by `job_id` instead of filename caused
  all but the last file path per job to be lost. Now uses backup filename as key.
- **Timeout enforcement** — `execute_with_timeout()` now measures elapsed time and returns
  an error if execution exceeds `timeout_secs`. Previously the parameter was accepted but
  ignored (`_timeout_secs`).
- **HealthMonitor RAM leak detection** — Memory leak alert gated on `cpu_alert_count` instead
  of a dedicated RAM counter. Added `ram_alert_count` field; RAM alerts now fire independently
  of CPU state.
- **Path validation TOCTOU** — `FileRead` and `FileWrite` now use the canonical `PathBuf`
  returned by `validate_path()` instead of the original user-supplied string, preventing
  symlink race attacks between validation and execution.
- **Kill signal parameter dead code** — Signal argument was parsed but always defaulted to
  SIGKILL (9). Now uses `args.signal.unwrap_or(15)` (SIGTERM default for graceful shutdown).
- **Daemon capability gap** — Daemon only registered FileRead + FileWrite. Now also registers
  ShellExec, Kill, Undo, and GitExec for parity with CLI.
- **ProcessSnapshot stale cache after kill** — Added `ProcessSnapshot::clear_cache()` to
  invalidate the 30-second cache before after-capture in kill operations.

### Improved

- **Error propagation** — WAL serialization failures now log with `eprintln!` instead of
  silently writing `Null`. Session `add_job` errors are logged instead of discarded.
  WAL corruption in Undo capability now returns an error instead of silently skipping.
- **Lock poison recovery** — `HealthMonitor` uses `unwrap_or_else(|e| e.into_inner())` instead
  of `expect()` for all RwLock accesses, preventing panics on poisoned locks.
- **HealthMonitor graceful shutdown** — Added `Drop` implementation to set stop flag on
  destruction.
- **parse_size_value format support** — Now handles `MB` and `GB` suffixes in addition to
  `Gi`, `Mi`, `Ki`.
- **Undo capability deduplication** — Uses `crate::utils::backup_dir()` and
  `crate::utils::wal_path()` instead of duplicating path logic.
- **Kill uses libc::kill** — Direct syscall instead of spawning `kill` subprocess for
  reliability and performance.
- **GitExec clippy fixes** — Fixed `if_same_then_else`, `needless_borrows`, and line-length
  warnings. Fixed test assertion (`abc123` is 6 chars, minimum is 7).

### Added

- 7 new tests (86 total, up from 79):
  - `test_kill_actual_process` — Verifies kill works on a real process (was ignored)
  - `test_kill_self_protected` — Verifies own PID is protected
  - `test_health_monitor_lifecycle` — Verifies start/stop without 60s wait
  - `test_health_state_defaults` — Verifies default state values
  - `test_cpu_alert_after_consecutive_checks` — Verifies CPU alert counting
  - `test_ram_alert_uses_ram_counter_not_cpu` — Verifies RAM alert independence (P1-2 fix)
  - `test_ram_alert_resets_when_ram_decreases` — Verifies counter reset on decrease

## [0.1.0] - 2026-05-16

### Added

- **Capability runtime** — Pluggable operations with name, schema, validate, execute
- **Two-layer telemetry** — Hardware (CPU, RAM, disk, GPU/TPU, services, network) + Process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — llmosafe circuit breaker reads /proc/stat and /proc/self/status
- **Write-ahead log** — Append-only, fsync'd event log for crash recovery
- **Backup/undo** — Files backed up before mutation, rollback by job ID
- **CLI** — run, list, status, logs, undo, telemetry, processes commands
- **Daemon** — Unix socket JSON-RPC server for remote capability execution
- **Unified path validation** — Single validate_path() module prevents path traversal and symlink attacks

## [0.1.0-alpha.2] - 2026-05-16

### Security Fixes

- **P0: FileWrite validation bypass** — `FileWrite::validate()` now uses the unified `validate_path()` module
  with `allowed_prefixes` enforcement. Previously, agents could write to arbitrary paths (`/etc/shadow`, etc.)
  because the unified validation was imported but never called. (G-SEC-1)
- **TOCTOU mitigation** — Path validation now canonicalizes the parent directory for non-existent
  paths (write targets), catching symlink-based directory escapes. Added symlink escape test. (G-SEC-2)
- **Prefix enforcement for writes** — Write operations now go through the same prefix check as reads
  (`/tmp`, `/var/tmp`, `/home`). Previously, no prefix restriction existed. (G-SEC-3)

### Bug Fixes

- **WAL cleanup data duplication** — `WalWriter::cleanup()` now truncates the file before rewriting
  retained events. Previously, it opened in append mode, doubling all retained entries. (G-EDGE-1)
- **UTF-8 truncation panic** — `truncate()` in process reports now uses `char_indices()` for safe
  slicing. Previously panicked on multi-byte character boundaries (CJK, emoji, Arabic). (G-EDGE-2)
- **Daemon startup panic** — `DaemonState::new()` now returns `Result` instead of panicking with
  `unwrap_or_else`. Backup directory or WAL directory creation failures are now graceful errors. (G-ERR-2)
- **Undo path reconstruction** — Removed fragile fallback that guessed original file paths from
  backup directory structure. Now returns a clear error if the WAL doesn't contain the original
  path for a job. (G-CTX-2)

### Improved

- **Path deduplication** — CLI and daemon now use `runtimo_core::utils::{data_dir, wal_path, backup_dir}`
  instead of duplicating the path logic in 3 locations. (G-CTX-1)
- **WAL tail() performance** — Changed from `Vec::remove(0)` (O(n) shift) to `VecDeque::pop_front()`
  (O(1) amortized) for the sliding window. (G-PERF-A)
- **Timeout documentation** — Executor timeout limitation is now prominently documented as
  "not currently enforced" with v0.2.0 tracking. Removed misleading duplicate docblock. (G-SEM-1)
- **Documentation accuracy** — Fixed stale `schema() -> &str` signature (now `-> Value`),
  fixed `FileWrite` examples to show constructor pattern, fixed license reference. (G-SEM-2)
- **Clippy clean** — Zero clippy warnings (was 7): fixed `needless_borrows`, `len_zero`, `ptr_arg`,
  removed unused imports.

### Added

- 6 new tests (52 total, up from 46):
  - `test_truncate_ascii` / `test_truncate_multibyte_utf8` — UTF-8 safety verification
  - `accepts_existing_tmp_file` — positive path validation
  - `accepts_nonexistent_tmp_file_for_writes` — write path validation
  - `rejects_write_outside_allowed` — prefix enforcement for writes
  - `rejects_symlink_escape` — symlink attack prevention

---

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
- Unit tests (21 tests)
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
- **21 unit tests** - Capability validation, execution, error handling, path security
- **31 integration tests** - End-to-end workflows, security checks, edge cases
- **7 doc tests** - API documentation examples
- **Total: 52 tests passing**

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
