# Runtimo Architecture

**Version:** 0.10.1
**Last Updated:** 2026-09-22

---

## Execution Pipeline

Every capability execution follows a 13-step pipeline:

```rust
// core/src/executor.rs — execute_with_telemetry_and_session()
// daemon single-source working_dir: daemon/src/engine.rs resolve_working_dir()

1. Telemetry::capture()               // hardware + service discovery
2. ProcessSnapshot::capture()         // process list with PPIDs
3. LlmoSafeGuard::check()             // /proc/stat, /proc/self/status, 80% ceiling
4. args size check                    // reject > 1 MB
5. zombie check                       // reject if > 10 zombies
6. WalWriter::append(JobStarted)      // fsync'd JSONL (one fsync per batch + watermark on bundle finalize; see core/src/observe/bundle.rs)
7. Safety assessment (if applicable)  // LLMOSafe 0.9 boundary: one-shot sifter or resource-only
8. WalWriter::append(SafetyEvaluated) // typed assessment evidence, before governed side effect
9. capability.validate()              // schema + path + semantic checks
10. capability.execute()               // runs the capability; Err-path still emits BackupCreated before JobFailed covering path/repo_path/dir (core/src/executor.rs)
11. Telemetry::capture()               // after snapshot
12. ProcessSnapshot::capture()        // after snapshot
13. WalWriter::append(JobCompleted)   // fsync'd, with output + telemetry; watermark fsync guarantees durability on bundle close
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
   │     single fresh upstream observation per call, no cache/cooldown
   ├── Safety boundary (core/src/safety.rs)
   │     LLMOSafe 0.9 conformance: InputClass, AnalysisKind, RuntimoDisposition, SafetyAssessmentV1
  ├── Telemetry (core/src/telemetry.rs)
  │     discovery-based: accelerators, services, system, network
  ├── Process Snapshot (core/src/processes.rs)
  │     ps aux parsing, PPID tracking, zombie detection
   ├── Config (core/src/config.rs)
  │     TOML at ~/.config/runtimo/config.toml, env var override; guards resolved via single-source RuntimoConfig::resolved() (top-level > [guards].* > profile != ephemeral) — core/src/config.rs
   ├── Observe Bundle (core/src/observe/bundle.rs)
  │     WAL-backed BundleWriter: 256 events or 100 ms batch, sha256(prev_hash ++ batch_json) hash chain, one fsync per batch + watermark fsync on finalize/drop; overflow injects ObserveTruncated with bundle_dropped sentinel; verify returns watermark Complete/Truncated/Incomplete from ObserveCompleted output.watermark
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
│     ├─ on timeout: kill(-pgid, SIGKILL) → wait; returns WaitOutcome { timed_out:true, signal:Some(9) } with partial output (core/src/capabilities/shell_exec.rs)
│     └─ on child exit: check descendants via /proc/{pid}/children; signal paths return timed_out:false, signal:Some(n) (core/src/capabilities/shell_exec.rs)
  ├─ Output data: { timed_out: bool (true ONLY on timeout-kill), signal: Option<i32>, exit_code, stdout, stderr, pid, timeout_secs, truncated }
  ├─ Telemetry capture (before + after)
  └─ [debug] WalWriter::append(CommandExecuted) with cmd, stdout, stderr, exit_code

## Daemon Working-Dir (single-source) + Observe Burst / Reconcile

```
daemon/src/engine.rs resolve_working_dir()  →  -32602 on invalid, never eprintln!+None (dual-decision→single-source fix, T2)
  ├─ handle_run / handle_dispatch / handle_observe_start all delegate to resolve_working_dir (no fallback to current_dir().unwrap_or("/"))
  └─ invalid working_dir never swallows to None

observe_start with burst:true → -32601 observe_burst deferred (daemon/src/engine.rs, daemon/src/rpc.rs)
observe_burst RPC → always -32601 deferred

reconcile (daemon restart): JobFailed preserves original capability + output {"reconciled": true} (daemon/src/engine.rs)
```
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
| 8 | Resource pressure → execution | `LlmoSafeGuard.check()` — 80% ceiling, single fresh upstream observation per call (no rolling average, no cooldown cache) |
| 9 | 1 MB args → memory | Executor pre-check rejects oversized args |
| 10 | Zombie count → execution | Executor rejects if zombie_count > 10 |

## Safety Architecture Invariants

### Provenance Ordering (ii)

Upstream `DecisionProvenance` is `Option`-typed and `None` on sifter-only and resource-only paths — never synthesized (`core/src/safety.rs:446-450,483`). The final typed `SafetyDecision` is canonical; `provenance.decision_label` is never the authority. `has_bias` (OR) is never dual-root (AND) evidence.

### `SafetyEvaluated` Durability (iii)

`SafetyEvaluated` WAL persistence is REQUIRED before the governed side effect (`core/src/executor.rs:508-541`). Append failure returns `Err(Error::WalError(...))` and blocks execution — WAL failure implies no side effect. Ordering: `JobStarted` → gates → `SafetyEvaluated` → `capability.execute()` → `JobCompleted/Failed`.

### Oracle Evaluator Shape (v)

`WalSource` and `RuntimeFactSource` share one narrow generic select-then-quantify evaluator (`core/src/oracle/generic_eval.rs`) over separate native typed field resolvers. No fake `WalEvent` adaptation of `RuntimeFactV1`. Selector field absence filters items out (never `Error`); predicate field absence or type mismatch yields `Verdict::Error`.
