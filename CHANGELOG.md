# Changelog

All notable changes to Runtimo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Daemon auto-start on dispatch** — `runtimo dispatch` now detects whether the daemon is running
  and starts it automatically if needed. The daemon binary is located relative to the CLI. No manual
  daemon management required. (`cli/src/main.rs`)
- **Daemon periodic maintenance** — background task (1-hour interval) now wires previously uncalled
  infrastructure: `WalWriter::cleanup()`, `WalWriter::rotate()`, and `BackupManager::cleanup()`.
  Prevents unbounded WAL and backup directory growth in long-running deployments.
  (`daemon/src/main.rs`)

### Fixed

- **Kill PID reuse race window** — start-time is now re-read and compared before sending the signal,
  narrowing the TOCTOU window where a recycled PID could receive a signal intended for a different
  process. (`core/src/capabilities/kill.rs`)

### Changed

- **SchemaValidator made private** — `pub mod schema` reduced to `mod schema`. The module is
  preserved for future wiring into the capability validation pipeline. (`core/src/lib.rs`,
  `core/src/schema.rs`)

### Security Fixes

- **CBP-1: Dispatch path bypassed safety checks** — `handle_dispatch` created a separate
  `CapabilityRegistry` and called `capability.execute()` directly, bypassing all 6 safety
  guards that `handle_run` provides via `execute_with_telemetry()` (LlmoSafeGuard resource
  check, zombie count check, args size check, telemetry capture, spawned PID tracking,
  timeout enforcement). Fixed by routing dispatch through `execute_with_telemetry()` with
  the daemon's canonical registry, removing the duplicated execution path. This eliminates
  compound symptoms: dual CapabilityRegistry (registry mismatch), working_dir silent drop
  (lost context), and TOCTOU window on working_dir validation.
  (`daemon/src/main.rs`)

## [0.5.0] - 2026-05-30

### Security Fixes

- **S-DEFAULT: Eliminated /tmp config fallback** — `config_path()` and `resource_history_path()` no
  longer fall back to world-writable `/tmp` when `XDG_CONFIG_HOME`/`HOME` are unset. Now panic with
  a clear message instead of writing persistent config to a shared temp directory (symlink attack
  vector, TOCTOU race, info leak). (`config.rs`, `llmosafe.rs`)

### Breaking Changes

- **Removed 10 write-only telemetry fields** — `ram_total_bytes`, `ram_free_bytes`,
  `disk_total_bytes`, `disk_free_bytes`, `disk_used_percent_numeric` (SystemInfo), `tpu_devices`,
  `gpu_devices` (HardwareInfo), `vllm_version`, `vllm_running`, `vllm_port_bound` (ServiceInfo)
  removed. Compute from `accelerators` vec or string fields instead. (`telemetry.rs`)
- **Removed 7 dead public exports** — `Job`, `ExecutionResult`, `HealthAlert`, `HealthState`,
  `Session`, `SessionManager` removed from `pub use` in lib.rs. Use qualified module paths
  (`runtimo_core::job::Job`, etc.) for internal access. (`lib.rs`)
- **Moved `format` module from core to cli** — `wall_to_markdown()` is a CLI presentation concern,
  not a core library function. Import from `cli/src/format.rs` instead of `runtimo_core::format`.
  (`core/src/format.rs` deleted, `cli/src/format.rs` created)

### Added

- **`SessionError` variant** — New `Error::SessionError(String)` variant for session-specific
  failures. `SessionManager` methods now return `SessionError` instead of misusing `BackupError`.
  (`lib.rs`, `session.rs`)
- **`rust-toolchain.toml`** — Pins stable toolchain with rustfmt + clippy components.
- **`cargo-deny` config** — `deny.toml` enforces license allowlist (MIT, Apache-2.0, BSD, ISC,
  Unicode-DFS-2016, Zlib), bans wildcard versions, audits for advisories.
- **MSRV CI testing** — `ci.yml` now tests against Rust 1.70.0 (declared MSRV).
- **`cargo-machete` CI** — Detects unused dependencies.
- **CI rust-cache** — All CI jobs now use `Swatinem/rust-cache@v2` for faster builds.

### Fixed

- **S-ENTROPY: session.rs BackupError misuse** — All 7 error sites in `SessionManager` now use
  `SessionError` instead of `BackupError`. Doc comments updated to match. (`session.rs`)
- **S-ORPHAN: Stale docs** — `RUNTIMO_CORE_LIB.rs` updated: Capability trait now shows
  `description()` method, built-in capabilities list complete (6 capabilities), Error enum
  includes `SessionError`, resource limits corrected (80% threshold, zombie count), Context
  example uses `Context::new()`, module list complete, version updated to 0.4.1. (`docs/`)
- **Rustdoc warnings** — Fixed 5 unclosed HTML tag warnings in daemon doc comments. (`daemon/src/main.rs`)
- **Telemetry demo** — Updated `telemetry_demo.rs` to compute GPU/TPU counts from accelerators
  vec instead of removed fields. (`core/examples/telemetry_demo.rs`)

### Changed

- **CI workflow expanded** — Added `msrv`, `deny`, `machete` jobs. Added `rust-cache` to all
  existing jobs. (`ci.yml`)
- **.gitignore hardened** — Added `AGENTS.md`, `references/`, `.cursor/`, `.claude/` to prevent
  agentic artifact leaks. (`.gitignore`)

## [0.4.0] - 2026-05-29

### Fixed

- **G-EDGE-1: check_disk_space runs before create_dir_all** — `check_disk_space()` was called
  before `create_dir_all()` in FileWrite, causing writes to nonexistent parent directories to
  fail with "No such file or directory" from `df`. Added early return when parent doesn't exist.
- **format_size double conversion** — `format_size()` else branch divided KB input by 1024,
  causing a 512KB process to display as "0K". Removed erroneous division.
- **parse_ps_line vsz/rss multiplied by 1024** — `parse_ps_line()` multiplied VSZ and RSS
  by 1024 (KB→bytes), but `format_size()` expects KB. A 1GB process displayed as 1024GB.
  Removed the multiplication.
- **vllm service hallucination** — `pgrep -f 'vllm serve'` matched the shell command itself,
  causing vllm to appear in telemetry output even when not running. Replaced process-based
  detection with port-based discovery.

### Changed

- **Telemetry service detection rewritten** — From hardcoded `pgrep` checks to port-based
  discovery. `ss -ltnp` scans listening TCP ports; ports mapped to known services via lookup
  table (ssh:22, nginx:80/443, postgres:5432, redis:6379, mysql:3306, mongodb:27017).
  Unknown ports are ignored. Version detected via service-specific commands.
- **Telemetry cache TTL increased** — 5s → 30s to accommodate service version detection
  strategies.
- **Process snapshot docs corrected** — "ps aux" references updated to "ps with explicit
  format" (code uses `ps -eo`, not BSD-style `ps aux`).
- **ServiceInfo docs corrected** — "Scans for known service processes" → "Scans for listening
  TCP ports". "No hardcoded service names" → "No assumed running services".

### Added

- **Enriched test suite (15 new tests)** — G-EDGE boundary tests for check_disk_space
  (nonexistent parent, deep nesting, single new parent, existing parent, empty content),
  C4 ordering dependency tests (concurrent paths, write after parent creation), G-SEM
  semantic invariants (content identity, unicode roundtrip), T-FALSEPASS regression tests
  (ordering sentinel, exact content check, tight oracle check).
- **process format_size unit test** — Validates all K/M/G branches.
- **process vsz/rss range test** — Regression sentinel for the `*1024` bug.
- **drift_process_vsz_rss_reasonable robust test** — Ensures VSZ/RSS stay in reasonable KB range.

## [0.2.0] - 2026-05-18

### Security Fixes (P0 - Critical)

**ShellExec:**
- Fixed unbounded output → OOM vulnerability (lines 106-107, 114-115, 312-313)
- Fixed `/bin/sh -c` bypass allowing arbitrary code execution (lines 273-280)
- Fixed `child.kill()` not killing descendants (line 93)
- Fixed pipe deadlock after kill (lines 93-107)
- Added dangerous command blocklist (`rm -rf /`, `dd`, `mkfs`, `fdisk`, `shutdown`, `chmod 777 /`)
- Added PATH hijack protection (requires absolute paths)

**FileWrite:**
- Fixed prefix validation bypass (`/tmpfoo/` passed `/tmp` check) in `path.rs:174`
- Added atomic write pattern (write to `.tmp`, fsync, rename)
- Added critical file denylist (`.bashrc`, `.ssh/authorized_keys`, `.profile`, etc.)
- Added disk space pre-check before writes

**GitExec:**
- Enforced `timeout_secs` on ALL git operations (was ignored before)
- Blocked `http://` URLs (MITM risk) - now requires `https://` or SSH
- Sanitized credentials from remote URLs in output
- Added safeguards to `git clean -fd` (dry-run preview, file count limit)
- Added secret file detection to `git add -A` (skips `.env`, `credentials.json`, etc.)
- Fixed URL validation bypasses (SSRF to cloud metadata)

**FileRead:**
- Fixed TOCTOU symlink escape (lines 128-153) using `O_NOFOLLOW`
- Fixed TOCTOU size bypass (lines 131-153) with bounded reader
- Added binary file detection
- Fixed UTF-8 truncation on multibyte boundaries

**WAL:**
- Fixed atomic write pattern (lock held during entire write)
- Fixed O(N²) I/O (now uses append mode)
- Fixed WAL rotation naming
- Fixed cleanup race window

**Backup:**
- Fixed restore to create pre-restore backup (was destroying newer data)
- Fixed `cleanup` symlink attack (was using `is_dir()` which follows symlinks)
- Added 100MB size limit per file
- Added backup integrity verification

**Executor:**
- Implemented zombie guard (reject if `zombie_count > 10`)
- Added args size limit (1MB max)
- Added spawned PID tracking
- Fixed WAL serialization to return error instead of storing `Null`

**Telemetry:**
- Added numeric metrics for agents (`disk_total_bytes`, `disk_free_bytes`, etc.)
- Reduced cache TTL from 30s to 5s
- Fixed `disk_used_percent` to store numeric value (stripped `%` sign)

### Features

- **Agent-friendly output:** JSON files auto-parsed to structured objects
- **Process groups:** ShellExec uses process groups for proper cleanup
- **Stdin support:** ShellExec now accepts stdin input
- **Output truncation:** 10MB limit on stdout/stderr with truncation flag
- **Numeric telemetry:** Agents can now compute thresholds programmatically

### Tests

- Added 50+ new tests across all capabilities
- All P0/P1/P2 audit issues have corresponding tests
- Total: 119 tests passing

### Documentation

- Updated README with correct binary name (`runtimo` not `moe`)
- Added comprehensive security documentation
- Documented all failure modes and mitigations

## [0.1.5] - 2026-05-17

### Bug Fixes

- **`create_backup` fails on directories** — `BackupManager::create_backup()` used `std::fs::copy()` which only works for regular files. When backing up a git repository (directory), it failed with "the source path is neither a regular file nor a symlink to a regular file". Replaced with `copy_recursive()` that handles both files and directories.

### Security

- **Symlink attack vector in backup** — `copy_recursive()` now explicitly rejects symlinks using `symlink_metadata()` to prevent symlink attack vectors where an attacker could place symlinks to sensitive files (e.g., `/etc/passwd`) in a directory being backed up.

### Features

- **Directory backup support** — `create_backup()` now recursively copies entire directory trees, enabling backup of git repositories and other directory structures.
- **Directory restore support** — `restore()` now uses `copy_recursive()` to restore both files and directories from backup.
- **Permission preservation** — On Unix systems, file and directory permissions (including executable bits) are preserved during backup/restore operations.

### Tests

- Added `test_backup_directory()` — Verifies directory tree backup works correctly
- Added `test_backup_rejects_symlinks()` — Verifies symlinks are rejected for security
- Added `test_restore_directory()` — Verifies directory restore from backup
- Added `test_backup_preserves_executable_bit()` — Verifies Unix permission preservation

## [0.1.4] - 2026-05-17

### Added

- **Persistent config file** — `~/.config/runtimo/config.toml` stores allowed path prefixes
  across invocations. Merged with built-in defaults and `RUNTIMO_ALLOWED_PATHS` env var.
- **CLI config subcommand** — `moe config allowed-paths add/list/remove` manages persistent
  path prefixes without needing env vars on every SSH invocation.
- **31 robust tests** covering 6 LLM failure mode categories:
  - **G-EDGE**: Empty content, single chars, long filenames, null bytes, concurrent writes
  - **G-SEC**: Encoded path traversal, null byte injection, symlink chains, type confusion,
    adversarial JSON
  - **G-ERR**: Directory reads, read-only locations, WAL failures, backup errors, invalid signals
  - **G-CTX**: Config file loading, env var precedence, invalid TOML handling, path validation
  - **G-SEM**: Backup numbering, WAL monotonicity, telemetry ordering, process consistency
  - **G-DRIFT**: Telemetry/process/WAL format stability via golden file assertions
- **4 property-based tests** (proptest):
  - Write/read roundtrip for arbitrary strings
  - Backup numbering produces no duplicates
  - Path validation is consistent for equivalent paths
  - WAL cleanup preserves recent events

### Dependencies

- Added `toml = "0.8"` for config file serialization
- Added `proptest = "1.4"` for property-based testing (dev-dependency)

## [0.1.3] - 2026-05-17

### Added

- **Capability descriptions** — All 6 capabilities now implement `description()` on the
  `Capability` trait. CLI `list` command shows human-readable descriptions alongside names.
- **CLI help text** — Every subcommand now has `about`, `long_about`, and `after_help` with
  usage examples. `moe --help` shows quick-start commands.
- **Timeout warning** — `execute_with_timeout_check` (renamed from `execute_with_timeout`)
  now emits an `eprintln!` warning when a capability exceeds its timeout, making the advisory
  nature visible to operators.
- **2 new tests** (86 total):
  - `respects_dry_run` — ShellExec returns immediately without spawning a process
  - `test_kill_dry_run` — Kill reports what would be killed without sending a signal

### Bug Fixes

- **Double `%%` in CLI disk output** — Format string `"disk: {}%"` appended `%` to a value
  that already included it from `df`. Removed the extra `%` from the format string.
- **Dead `unpark()` in HealthMonitor Drop** — `self._thread.thread().unpark()` called on a
  `JoinHandle` that never parks. Removed dead code.
- **Curl hang in telemetry** — `NetworkInfo::capture()` called `curl ifconfig.me` with no
  timeout. Added `--connect-timeout 5 --max-time 5` to prevent indefinite hangs.
- **`parse_ram_percent` always returned 0%** — Function accepted a single formatted string
  (`"16Gi total, 13Gi free"`) and tried to parse both values from it. Changed signature to
  `parse_ram_percent(ram_total, ram_free)` accepting separate telemetry fields.
- **ID collision from timestamp-based IDs** — `JobId::new()` and `SessionManager::create_session()`
  used nanosecond timestamps, causing collisions under concurrent execution. Replaced with
  `utils::generate_id()` using 16 bytes from `/dev/urandom` (32 hex chars, fallback to timestamp).
- **Backup overwrite lost original state** — `BackupManager::create_backup()` overwrote the
  backup file on repeated writes within the same job. Now appends numeric suffixes (`.1`, `.2`)
  so the first backup always contains the true original.
- **WAL cleanup race condition** — `WalWriter::cleanup()` truncated then rewrote the WAL,
  losing events appended by concurrent writers during the window. Now writes to a temp file,
  re-reads the original to merge any concurrent appends (by `seq` comparison), then atomically
  renames.
- **Dead code in `parse_ps_line`** — `parts.get(10).unwrap_or(&"").to_string()` was always
  `None` when `parts.len() == 10`. Replaced with `parts.get(10..).map(...).unwrap_or_default()`.

### Audit Findings Fixed

- **G-CTX-1: ShellExec ignores `dry_run`** — `ShellExec.execute` now checks `ctx.dry_run`
  and returns immediately without spawning any process.
- **G-CTX-2: Kill ignores `dry_run`** — `Kill.execute` now checks `ctx.dry_run` and reports
  what would be killed (with process snapshot) without sending a signal.
- **G-EDGE-1: WAL cleanup concurrent-append race** — Fixed (see Bug Fixes above).

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
- v0.1.3 (2026-05-17) - Bug fixes, audit findings, capability descriptions, CLI help text
- v0.1.2 (2026-05-16) - Telemetry + process tracking, 8 bug fixes, code audit remediation
- v0.1.1 (2026-05-16) - Security hardening, daemon auth, Kill/GitExec wiring
- v0.1.0 (2026-05-16) - Initial stable release with docs, release workflow, and runner fixes
