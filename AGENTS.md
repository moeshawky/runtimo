# Runtimo — Agent Implementation Guide

**Purpose:** Guide AI agents implementing Runtimo capabilities  
**Core Identity:** Capability runtime for **persistent machines** (cannot factory reset)  
**Last Updated:** 2026-05-20 (dev-only CommandExecuted WAL + error-absorbing logging)  
**Verified:** 176 tests pass, clippy clean, release build zero warnings

---

## [INTENT COMPRESSED]

**Signal:** Execute capabilities safely with two-layer telemetry (hardware + process) on persistent machines.

**Decoded:**
- Agents hallucinate → Runtimo validates inputs
- Machines are persistent → Cannot reset, must track processes
- Data at stake → Log everything to WAL, enable undo
- First milestone: FileRead with telemetry + process tracking

**Success Criteria:**
1. Every execution captures hardware telemetry (CPU, RAM, disk, services)
2. Every execution captures process snapshot (ps aux, zombies, spawned PIDs)
3. Resource guards enforced (CPU >90% = reject, RAM >90% = reject)
4. All events logged to WAL with before/after snapshots
5. `moe run FileRead --args '{"path":"/tmp/test.txt"}'` executes with full telemetry

---

## [ROLE ARCHITECTURE]

### Layer 1: Domain Expert
You are a **Rust runtime engineer** specializing in:
- Capability-based security + resource-aware execution
- WAL-style crash recovery on persistent machines
- Two-layer telemetry (hardware + process tracking)
- Agent hallucination absorption via validation

### Layer 2: Task Specialist
**Your task:** Implement capabilities following scaffolded architecture with mandatory telemetry.

**Constraints:**
- Use existing scaffold (core/src/capability.rs, core/src/job.rs)
- Follow Capability trait (name, schema, validate, execute)
- **MANDATORY:** Capture telemetry + process snapshot before/after
- Integrate with WAL logging (core/src/wal.rs)
- No external dependencies beyond workspace

### Layer 3: Constraint Enforcer
**HARD (Non-negotiable):**
1. **NO TELEMETRY, NO EXECUTION** — Must capture hardware + process before ANY capability
2. Must validate args against JSON schema before execution
3. Must log all events to WAL (fsync after each)
4. Must not panic — return Result<Error> on failure
5. Resource guards are hard limits: CPU >90% = reject, RAM >90% = reject, zombies >10 = alert

**SOFT (Preferred):**
- Use existing patterns from moesniper/moeix (hex encoding, dry-run)
- Keep implementation under 100 lines
- Add docstring with example usage

### Layer 4: Output Format
```json
{
  "success": true/false,
  "output": "...",
  "telemetry_before": {"cpu": "AMD EPYC", "ram_free": "13Gi", ...},
  "telemetry_after": {...},
  "process_before": {"count": 176, "zombies": 0, "top_cpu": "opencode"},
  "process_after": {"count": 177, "spawned": [12345, 12346]},
  "wal_event_id": "abc123",
  "resource_usage": {"cpu_time_ms": 150, "memory_peak_mb": 45}
}
```

---

## [CONTEXT]

### Current State (Measured)
| Component | Status | Lines | Notes |
|-----------|--------|-------|-------|
| `core/src/lib.rs` | ✅ Done | 50 | Exports incl. Telemetry, ProcessSnapshot |
| `core/src/job.rs` | ✅ Done | 90 | Job lifecycle, state machine |
| `core/src/capability.rs` | ✅ Done | 60 | Capability trait, registry |
| `core/src/schema.rs` | ✅ Done | 40 | JSON Schema validation |
| `core/src/wal.rs` | ✅ Done | 200+ | WAL writer, reader, rotation, CommandExecuted events |
| `core/src/backup.rs` | ✅ Done | 100+ | Backup manager (undo, dir backup, permission preserve) |
| `core/src/llmosafe.rs` | ✅ Done | 50 | Resource guard |
| `core/src/telemetry.rs` | ✅ Done | 200+ | Hardware telemetry |
| `core/src/processes.rs` | ✅ Done | 200+ | Process snapshot with PPID tracking |
| `core/src/executor.rs` | ✅ Done | 100+ | CommandExecuted WAL logging (dev-only) |
| `core/src/config.rs` | ✅ Done | 50 | Persistent TOML config |
| `core/src/session.rs` | ✅ Done | 100+ | Session tracking |
| `core/src/monitor.rs` | ✅ Done | 100+ | Health monitor (snapshots, alerts) |
| `core/src/capabilities/shell_exec.rs` | ✅ Done | 200+ | ShellExec: sh -c, timeout, blocklist, WAL audit |
| `core/src/capabilities/kill.rs` | ✅ Done | 100+ | Kill: POSIX signals, PID reuse protection |
| `core/src/capabilities/git_exec.rs` | ✅ Done | 200+ | Git operations with URL sanitization |
| `core/src/capabilities/file_read.rs` | ✅ Done | 200+ | FileRead: O_NOFOLLOW, binary detection, JSON parse |
| `core/src/capabilities/file_write.rs` | ✅ Done | 300+ | FileWrite: atomic write, backup, critical file deny |
| `core/src/capabilities/undo.rs` | ✅ Done | 50 | Undo: WAL-backed restore |
| `daemon/src/main.rs` | ⏳ Placeholder | 20 | Needs full JSON-RPC impl |
| `cli/src/main.rs` | ✅ Done | 200+ | CLI with compiler-error style help |

### Prior Attempts (Stigmergic Signals)
**Success Pattern:** FileRead followed Capability trait → clean integration  
**Failure Signature:** Don't skip telemetry — causes blindness on persistent machines  
**Constraint Evolution:** Originally no telemetry, now MANDATORY (2026-05-16)

### Failure Modes to Prevent
| Failure Mode | Prevention |
|--------------|------------|
| **R-HALL** (Hallucinated APIs) | "Only use std::fs, serde_json, runtimo-core. If unsure, say 'uncertain'." |
| **R-ASSUME** (Assumed state) | "List assumptions about system state before implementing." |
| **R-EDGE** (Edge cases) | "Handle: low RAM, high CPU, zombie processes, spawned runaways." |
| **R-CASCADE** (Compound) | "Check: WAL write failure, telemetry capture failure, process spawn explosion." |
| **R-SEC** (Security) | "Flag: path traversal, symlink attacks, unauthorized process spawning." |
| **R-PERF** (Performance) | "Note: execution must complete in <30s, memory <512MB." |

---

## [EXECUTION PROTOCOL]

### Before Execution (MANDATORY)
```rust
use runtimo_core::{Telemetry, ProcessSnapshot};

// 1. Capture hardware telemetry
let telemetry_before = Telemetry::capture();

// 2. Capture process snapshot
let process_before = ProcessSnapshot::capture();

// 3. Check thresholds
if process_before.summary.total_cpu_percent > 90.0 {
    return Err(Error::ResourceLimitExceeded("CPU > 90%".into()));
}
if process_before.summary.total_mem_percent > 90.0 {
    return Err(Error::ResourceLimitExceeded("RAM > 90%".into()));
}
if process_before.summary.zombie_count > 10 {
    alert_operator("Zombie processes > 10");
    return Err(Error::ResourceLimitExceeded("Zombies > 10".into()));
}

// 4. Log job_start to WAL
wal.log(WalEvent::JobStart { telemetry: &telemetry_before, process: &process_before });
```

### During Execution
```rust
// 5. Enforce llmosafe limits (CPU time, memory, timeout)
// 6. Track spawned child PIDs
// 7. Monitor for resource spikes
```

### After Execution
```rust
// 8. Capture after snapshots
let telemetry_after = Telemetry::capture();
let process_after = ProcessSnapshot::capture();

// 9. Identify spawned processes
let spawned_pids = identify_spawned(&process_before, &process_after);

// 10. Log job_complete to WAL
wal.log(WalEvent::JobComplete {
    telemetry_after,
    process_after,
    spawned_pids,
    success,
});
```

---

## [TASK: FileRead Implementation]

### Step 1: Create capability file
**Location:** `core/src/capabilities/file_read.rs`

```rust
use crate::{Capability, Context, Output, Error, Result, Telemetry, ProcessSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadArgs {
    pub path: String,
}

pub struct FileRead;

impl Capability for FileRead {
    fn name(&self) -> &'static str { "FileRead" }
    
    fn schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#
    }
    
    fn validate(&self, args: &serde_json::Value) -> Result<()> {
        let args: FileReadArgs = serde_json::from_value(args.clone())?;
        if args.path.is_empty() {
            return Err(Error::SchemaValidationFailed("path is empty".into()));
        }
        Ok(())
    }
    
    fn execute(&self, args: &serde_json::Value, _ctx: &Context) -> Result<Output> {
        // MANDATORY: Capture telemetry before
        let telemetry_before = Telemetry::capture();
        let process_before = ProcessSnapshot::capture();
        
        // Check thresholds
        if process_before.summary.total_cpu_percent > 90.0 {
            return Err(Error::ResourceLimitExceeded("CPU > 90%".into()));
        }
        
        // Execute capability
        let args: FileReadArgs = serde_json::from_value(args.clone())?;
        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| Error::ExecutionFailed(format!("Failed to read {}: {}", args.path, e)))?;
        
        // Capture after
        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();
        
        // Log to WAL (handled by framework)
        Ok(Output {
            success: true,
            data: serde_json::json!({"content": content}),
            telemetry_before,
            telemetry_after,
            process_before: process_before.summary.total_processes,
            process_after: process_after.summary.total_processes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_file_read_exists() {
        // Create temp file, read, assert content
    }
    
    #[test]
    fn test_file_read_not_found() {
        // Read non-existent file, assert error
    }
}
```

### Step 2-5: Export, Register, Test, Verify
(See original AGENTS.md for steps 2-5)

---

## [VERIFICATION]

### Unit Tests
```bash
cargo test -p runtimo-core -- file_read
# Expected: 2 tests pass (exists, not_found)
```

### Telemetry Verification
```bash
./target/debug/moe telemetry
# Expected: AMD EPYC, RAM free/disk used, services, network
```

### Process Verification
```bash
./target/debug/moe processes
# Expected: 176 processes, 0 zombies, top CPU/mem consumers
```

### Integration Test
```bash
echo "hello world" > /tmp/runtimo_test.txt
./target/debug/moe run FileRead --args '{"path":"/tmp/runtimo_test.txt"}'
# Expected: {"success":true,"data":{"content":"hello world\n"},...}
rm /tmp/runtimo_test.txt
```

### WAL Verification
```bash
cat /var/lib/runtimo/wal.jsonl | tail -1
# Expected: {"seq":1,"type":"job_completed","capability":"FileRead","telemetry_before":{...}}
```

---

## [MEMORY AIDS]

**Ephemeral Machine (Kaggle):** Factory reset OK, no process tracking  
**Persistent Machine (This box):** Cannot reset, MUST track processes

**Two-Layer Telemetry:**
1. Hardware: What exists (CPU, RAM, disk, TPU/GPU, services, network)
2. Process: What's running (ps aux, zombies, spawned PIDs)

**Success Pattern:**
Telemetry → Process Snapshot → Check Thresholds → Execute → Log WAL → Track Spawned

**Quick Commands:**
```bash
./target/debug/moe telemetry    # Hardware state
./target/debug/moe processes    # Running processes
./target/debug/moe run -c <cap> -a '{}'  # Execute capability
./target/debug/moe logs         # View WAL
kill <pid>                      # Kill runaway
```

---

## [NEXT TASKS]

After FileRead:
1. **FileWrite** (with backup for undo)
2. **FileExists** (boolean check)
3. **ShellExec** (sandboxed, timeout)
4. **Process kill capability** (kill spawned runaways)
5. **Health monitoring daemon** (periodic snapshots, alert on anomalies)

**Current Priority:** P0 - Job execution with telemetry + process tracking  
**TODO:** [`TODO.md`](./TODO.md) for complete task list

---

## [STIGMERIC SIGNALS]

**For future agents:**
- **Success Pattern:** Telemetry + process tracking → full environment awareness
- **Failure Signature:** Don't skip telemetry — blindness on persistent machines
- **Constraint Evolution:** Originally no telemetry, now MANDATORY (2026-05-16)
- **Progress Marker:** Scaffold complete → First capability (FileRead) next

**Memory Reference:** `memory://runtimo-persistent-machine-design`

---

**Document Source:** Scaffold measurement + telemetry implementation (2026-05-16)  
**Next Review:** After FileRead implementation complete  
**Verification Command:** `cargo test -p runtimo-core -- file_read && ./target/debug/moe telemetry && ./target/debug/moe processes`

*"On persistent machines, every capability leaves a trace. Your job is to follow the traces."*
