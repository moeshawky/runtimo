# Runtimo

**Capability runtime with telemetry, WAL, and process tracking.**

[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What Is Runtimo?

Runtimo is a Rust workspace providing a **capability execution engine**. Every capability execution is wrapped with:

- **Telemetry** — Hardware (CPU, RAM, disk, accelerators, services, network) + process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — `llmosafe` circuit breaker reads `/proc/stat` and `/proc/self/status`; rejects execution when pressure exceeds 80%
- **Write-ahead log** — Append-only, fsync'd JSONL event log with crash recovery
- **Backup/undo** — Files backed up before mutation, rollback by job ID
- **Input validation** — Capabilities validate arguments including path traversal, symlink, and null byte protection

**Version:** 0.6.5 | **Rust Edition:** 2021 | **Tests:** 233 (127 lib + 4 doc + 51 int + 40 robust + 7 cli)

## Quick Start

### Library

```bash
cargo add runtimo-core
```

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileRead;
let args = json!({"path": "/tmp/test.txt"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;

println!("Success: {}  Job: {}  WAL seq: {}", result.success, result.job_id, result.wal_seq);
```

### CLI

```bash
cargo build --release

# List capabilities
runtimo list

# Read a file
runtimo run -c FileRead -a '{"path":"/etc/hostname"}'

# Write a file (creates automatic backup)
runtimo run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'

# Shell command
runtimo run -c ShellExec -a '{"cmd":"ls | head -3"}'

# Dry run (validate without executing)
runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run

# View system telemetry
runtimo telemetry

# View process snapshot
runtimo processes

# View WAL events
runtimo logs

# Undo a job
runtimo undo -j <job_id>
```

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│ runtimo CLI                                                    │
│ (run, list, telemetry, processes, logs, status, undo, config)  │
└──────────────────────────┬─────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────┐
│ CapabilityRegistry                                             │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────┐ ┌──────┐ ┌─────────┐│
│ │ FileRead │ │FileWrite │ │ShellExec │ │ Undo │ │ Kill │ │ GitExec ││
│ └──────────┘ └──────────┘ └──────────┘ └──────┘ └──────┘ └─────────┘│
└──────────────────────────┬─────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────┐
│ execute_with_telemetry()                                       │
│                                                                │
│ 1. Telemetry::capture()         — hardware + service discovery │
│ 2. ProcessSnapshot::capture()   — process list with PPIDs      │
│ 3. LlmoSafeGuard::check()       — resource guard (80% ceiling) │
│ 4. WalWriter::append(Started)   — WAL event (fsync)            │
│ 5. capability.validate()        — schema + path checks         │
│ 6. capability.execute()         — run the capability           │
│ 7. Telemetry::capture()         — after snapshot               │
│ 8. ProcessSnapshot::capture()   — after snapshot               │
│ 9. WalWriter::append(Completed) — WAL event (fsync)            │
│                                                                │
│ Returns: ExecutionResult with before/after telemetry           │
└────────────────────────────────────────────────────────────────┘
```

## Available Capabilities

### FileRead

Read file contents. Validates path exists, is a file, no traversal.

| Field | Type | Required? |
|-------|------|-----------|
| `path` | string | yes |
| `max_bytes` | integer (1–10,485,760) | no |

**Limit:** 10 MB max file size. Binary files detected and base64-encoded. JSON files auto-parsed. UTF-8 boundary safe.

```bash
runtimo run -c FileRead -a '{"path":"/tmp/data.txt"}'
```

### FileWrite

Write file content with backup-before-mutate for undo support. Appends supported.

| Field | Type | Required? |
|-------|------|-----------|
| `path` | string | yes |
| `content` | string | yes |
| `append` | boolean | no |

**Limit:** 100 MB max content. 100 MB max cumulative file size for append. 10 MB minimum free disk required. Critical files (`.bashrc`, `.ssh/authorized_keys`, etc.) blocked.

```bash
runtimo run -c FileWrite -a '{"path":"/tmp/out.txt","content":"hello"}'
runtimo run -c FileWrite -a '{"path":"/tmp/log.txt","content":"\nline 2","append":true}'
```

### ShellExec

Execute shell commands via `sh -c`. Supports pipes, redirects, chaining, variables. Enforces timeout and dangerous command blocklist.

| Field | Type | Required? |
|-------|------|-----------|
| `cmd` | string | yes |
| `timeout_secs` | integer (1–300) | no |
| `cwd` | string | no |
| `stdin` | string | no |

**Guardrails:** Blocks `mkfs`, `fdisk`, `dd`, `shutdown`, `reboot`, `poweroff`, `rm -rf /` (root/dev/boot), `chmod 777 /`. Timeout default 30s, max 300s. Kills all child processes on timeout. Output capped at 10 MB.

```bash
runtimo run -c ShellExec -a '{"cmd":"uptime"}'
runtimo run -c ShellExec -a '{"cmd":"ls | head -5"}'
runtimo run -c ShellExec -a '{"cmd":"echo hi && whoami"}'
```

### Undo

Restore files from backup using job ID. Reads WAL to find original paths, validates restore targets against allowed prefixes.

| Field | Type | Required? |
|-------|------|-----------|
| `job_id` | string | yes |

```bash
runtimo undo -j abc123
```

### Kill

Terminate a process by PID with signal support. Protected PIDs (init, kthreadd, self, parent) cannot be killed. Includes PID reuse protection via `/proc/{pid}/stat` start-time comparison.

| Field | Type | Required? |
|-------|------|-----------|
| `pid` | integer (≥1) | yes |
| `signal` | integer (-64–64) | no |

```bash
runtimo run -c Kill -a '{"pid":12345}'
runtimo run -c Kill -a '{"pid":12345,"signal":9}'
```

### GitExec

Git operations (clone, pull, commit, revert, clean, status). URL sanitization, SSRF blocking, secret file detection, branch/commit validation.

| Field | Type | Required? |
|-------|------|-----------|
| `operation` | string (clone\|pull\|commit\|revert\|clean\|status) | yes |
| `url` | string | no |
| `path` | string | no |
| `branch` | string | no |
| `message` | string | no |
| `files` | array of strings | no |
| `commit_sha` | string | no |
| `timeout_secs` | integer (1–600) | no |

```bash
runtimo run -c GitExec -a '{"operation":"status","path":"/tmp/repo"}'
runtimo run -c GitExec -a '{"operation":"clone","url":"https://github.com/user/repo.git","path":"/tmp/repo"}'
```

## Safety Model

| Layer | Mechanism | What it does |
|-------|-----------|--------------|
| **Path validation** | `validate_path()` | Rejects traversal (`..`), null bytes, non-ASCII, symlink escapes. Enforces allowed prefix whitelist (`/tmp`, `/var/tmp`, `/home` + config). |
| **Critical file deny** | `is_critical_file()` | Blocks `.bashrc`, `.ssh/authorized_keys`, `.gitconfig`, `.netrc`, etc. |
| **Resource guard** | `LlmoSafeGuard` | Reads `/proc/stat` + `/proc/self/status`. Rejects execution when pressure > 80%. Rolling average over 30s. Cooldown persists across restarts. |
| **Zombie guard** | Executor pre-check | Rejects execution if zombie count > 10. |
| **Args size guard** | Executor pre-check | Rejects capability arguments > 1 MB. |
| **Disk space check** | `check_disk_space()` | Runs `df -B1`, parses header-aware "Available" column. Requires 10 MB free. |
| **WAL audit** | `WalWriter` | Every job start/completion/failure written to append-only JSONL with fsync. Sequence recovery, rotation, cleanup. |
| **Backup/undo** | `BackupManager` | Backup before mutate. Integrity verified (size comparison). Restore validates target paths against allowed prefixes. |
| **Shell timeout** | `wait_with_timeout()` | ShellExec kills entire process group on timeout. |
| **Kill protection** | Protected PID list + PID reuse check | init(1), kthreadd(2), self, and parent PIDs cannot be killed. Start-time comparison prevents wrong-target kills. |

## Telemetry

### Discovery-Based Detection

Telemetry detects what's running — no assumptions about hardware or services.

| Category | What it detects | How |
|----------|-----------------|-----|
| **CPU** | Model, count | `/proc/cpuinfo` |
| **RAM** | Total, free, available | `/proc/meminfo` |
| **Disk** | Total, used, available % | `df -h` |
| **Accelerators** | NVIDIA, AMD, TPU, DRM | `nvidia-smi`, `rocm-smi`, `/dev/accel*`, `/dev/dri/render*` |
| **Services** | vLLM, nginx, postgres, redis, docker | `pgrep` + version detection |
| **Network** | Public IP, interfaces, tunnel status | `curl`, `/sys/class/net/*` |
| **Processes** | Full list, zombies, top consumers, PPID chain | `ps aux` + `/proc` |

Unavailable hardware/services are simply absent from output — no "not installed" noise.

```bash
runtimo telemetry       # human-readable report
runtimo telemetry --json # machine-readable
```

## WAL Events

All events written to append-only JSONL with fsync:

| Event Type | When |
|------------|------|
| `job_started` | Before validation |
| `job_completed` | After successful execution |
| `job_failed` | On validation or execution failure |
| `command_executed` | (Debug builds only) Shell command with stdout/stderr/exit code |

```bash
runtimo logs                    # last 10 events
runtimo logs -n 50              # last 50 events
runtimo logs -j <job_id>        # filter by job
runtimo status                  # job summaries from WAL
```

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
│   │   ├── telemetry.rs    # Hardware + service discovery
│   │   ├── processes.rs    # Process snapshot with PPID tracking
│   │   ├── llmosafe.rs     # llmosafe ResourceGuard integration
│   │   ├── wal.rs          # Write-ahead log (WalWriter, WalReader)
│   │   ├── backup.rs       # BackupManager for undo support
│   │   ├── session.rs      # Session tracking and persistence
│   │   ├── config.rs       # TOML configuration + allowed paths
│   │   ├── monitor.rs      # Health monitor (snapshots, alerts)
│   │   ├── cmd.rs          # Shell command execution helper
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
│   ├── tests/
│   │   ├── integration.rs  # 31 integration tests
│   │   └── robust.rs       # 31 property-based tests (6 G-categories)
│   └── examples/
├── cli/                    # runtimo binary
│   └── src/
│       └── main.rs         # CLI commands via clap
└── daemon/                 # runtimo-daemon binary
    └── src/
        └── main.rs         # Future JSON-RPC server
```

## Testing

```bash
cargo test                           # all tests
cargo test -p runtimo-core --lib    # 120 unit tests
cargo test -p runtimo-core --test integration  # 31 integration tests
cargo test -p runtimo-core --test robust       # 31 property-based tests
cargo test -p runtimo-core --doc    # 24 doc tests
cargo clippy --all-targets          # zero warnings required
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `$XDG_DATA_HOME/runtimo/wal.jsonl` | WAL file path |
| `RUNTIMO_BACKUP_DIR` | `$XDG_DATA_HOME/runtimo/backups` | Backup directory |
| `RUNTIMO_SESSIONS_DIR` | `$XDG_DATA_HOME/runtimo/sessions` | Session storage |
| `RUNTIMO_ALLOWED_PATHS` | (colon-separated) | Additional allowed path prefixes |
| `XDG_CONFIG_HOME` | `~/.config` | Config file location (`runtimo/config.toml`) |
| `XDG_DATA_HOME` | `~/.local/share` | Default WAL/backup/session root |
| `RUNTIMO_ENABLE_PUBLIC_IP` | (unset) | Set to `1` to enable public IP discovery in telemetry |
| `RUNTIMO_DAL` | (unset) | Data access layer configuration (future) |
| `RUNTIMO_STATE_DIR` | `$XDG_DATA_HOME/runtimo` | Override state directory for WAL/backups/sessions |
| `RUNTIMO_ENABLE_NETWORK` | (unset) | Set to `1` to allow outbound network tools (curl, wget, etc.) in ShellExec |

## License

MIT — see [LICENSE](LICENSE).
