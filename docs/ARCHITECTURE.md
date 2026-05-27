# Runtimo Architecture

**Version:** 0.2.2
**Last Updated:** 2026-05-28

---

## Execution Pipeline

Every capability execution follows a 10-step pipeline:

```rust
// core/src/executor.rs — execute_with_telemetry_and_session()

1. Telemetry::capture()               // hardware + service discovery
2. ProcessSnapshot::capture()         // process list with PPIDs
3. LlmoSafeGuard::check()             // /proc/stat, /proc/self/status, 80% ceiling
4. args size check                    // reject > 1 MB
5. zombie check                       // reject if > 10 zombies
6. WalWriter::append(JobStarted)      // fsync'd JSONL
7. capability.validate()              // schema + path + semantic checks
8. capability.execute()               // runs the capability
9. Telemetry::capture()               // after snapshot
10. ProcessSnapshot::capture()        // after snapshot
11. WalWriter::append(JobCompleted)   // fsync'd, with output + telemetry
```

## Module Map

```
CLI (cli/src/main.rs)
  │ clap args → CapabilityRegistry → executor
  ▼
Executor (core/src/executor.rs)
  │ dispatch, guard, WAL logging
  ▼
Capability (core/src/capability.rs)
  │ trait dispatch → validate() → execute()
  ├── FileRead   (core/src/capabilities/file_read.rs)
  ├── FileWrite  (core/src/capabilities/file_write.rs)
  ├── ShellExec  (core/src/capabilities/shell_exec.rs)
  ├── Kill       (core/src/capabilities/kill.rs)
  ├── GitExec    (core/src/capabilities/git_exec.rs)
  └── Undo       (core/src/capabilities/undo.rs)
  │
  ├── Path Validation (core/src/validation/path.rs)
  │     traversal, null byte, symlink, prefix enforcement
  ├── Backup Manager (core/src/backup.rs)
  │     backup-before-mutate, integrity verify, restore with pre-restore backup
  ├── WAL (core/src/wal.rs)
  │     append-only JSONL, fsync, flock, rotation, cleanup, tail-read seq recovery
  ├── LlmoSafeGuard (core/src/llmosafe.rs)
  │     rolling average, cooldown, persisted to disk across restarts
  ├── Telemetry (core/src/telemetry.rs)
  │     discovery-based: accelerators, services, system, network
  ├── Process Snapshot (core/src/processes.rs)
  │     ps aux parsing, PPID tracking, zombie detection
  ├── Config (core/src/config.rs)
  │     TOML at ~/.config/runtimo/config.toml, env var override
  ├── Session Manager (core/src/session.rs)
  │     session create/list/add-job, persisted to disk
  └── Monitor (core/src/monitor.rs)
        background snapshots, CPU/RAM alert thresholds
```

## Data Flow: FileWrite

```
FileWrite.execute()
  │
  ├─ Telemetry::capture()              // before snapshot
  ├─ ProcessSnapshot::capture()        // before processes
  ├─ validate_path()                   // traversal, null byte, prefix
  ├─ is_critical_file()                // .bashrc, .ssh/authorized_keys, etc.
  ├─ check_disk_space()               // df -B1 → header-aware "Available" parse
  ├─ [if existing] BackupManager.create_backup() → copy_recursive → verify_integrity
  ├─ atomic_write() / atomic_append()  // write to .tmp → fsync → rename → dir sync
  ├─ Telemetry::capture()              // after snapshot
  └─ ProcessSnapshot::capture()        // after processes
```

## Data Flow: Undo

```
Undo.execute() / CLI undo
  │
  ├─ WalReader.load()                  // read WAL for backup→path mapping
  ├─ For each backup file:
  │   ├─ map backup_path → original_path (from WAL)
  │   ├─ validate_path(original_path)  // re-validate against allowed prefixes
  │   └─ BackupManager.restore(backup, original)
  │         ├─ pre-restore backup (current state saved)
  │         ├─ copy_recursive(backup → target)
  │         └─ overwrite completes
  └─ Output: list of restored paths
```

## Data Flow: ShellExec

```
ShellExec.execute()
  │
  ├─ is_dangerous_command()            // block mkfs, fdisk, dd, shutdown, rm -rf /
  ├─ Command::new("sh").arg("-c").arg(cmd)
  │     .stdin(pipe) .stdout(piped) .stderr(piped)
  ├─ setpgid() → process group isolation
  ├─ wait_with_timeout(child, pgid, timeout)
  │     ├─ read stdout/stderr (bounded to 10 MB each)
  │     ├─ on timeout: kill(-pgid, SIGKILL) → wait
  │     └─ on child exit: check descendants via /proc/{pid}/children
  ├─ Telemetry capture (before + after)
  └─ [debug] WalWriter::append(CommandExecuted) with cmd, stdout, stderr, exit_code
```

## Safety Boundaries

| # | Boundary | Mechanism |
|---|----------|-----------|
| 1 | User input → capability | `Capability::validate()` — schema + semantic checks |
| 2 | User input → filesystem path | `validate_path()` — traversal, null, symlink, prefix |
| 3 | FileWrite → disk | `check_disk_space()` + atomic write pattern |
| 4 | Shell command → system | Dangerous command blocklist + timeout + process group kill |
| 5 | Kill PID → process | Protected PID list + PID reuse detection |
| 6 | GitExec → network | URL validation (http/https/SSH) + SSRF blocking |
| 7 | Undo → filesystem | Restore target re-validated against allowed prefixes |
| 8 | Resource pressure → execution | `LlmoSafeGuard.check()` — 80% ceiling, rolling average |
| 9 | 1 MB args → memory | Executor pre-check rejects oversized args |
| 10 | Zombie count → execution | Executor rejects if zombie_count > 10 |
