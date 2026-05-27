# Runtimo Design Decisions

**Date:** 2026-05-16 (updated 2026-05-20)  
**Status:** Implemented  
**Core Insight:** Persistent machines require different awareness than ephemeral machines

## The Problem: Ephemeral vs Persistent Machines

### Ephemeral Machines (Kaggle, Colab, Notebooks)
- **Factory reset** with a button press
- Session ends = everything vanishes
- Telemetry = "What hardware exists?" (static snapshot)
- No need to track processes (they all die on reset)
- **Data at stake:** None (ephemeral)

### Persistent Machines (Dev box, servers, workstations)
- **Cannot factory reset** - data is at stake
- Sessions end, but processes may linger
- Telemetry must answer: "What's running? What's broken? What can I kill?"
- Process tracking is **critical** (not optional)
- **Data at stake:** Everything

## The Solution: Two-Layer Awareness

Runtimo implements **both** layers of telemetry:

### Layer 1: Hardware Telemetry (Ephemeral Pattern)

```rust
use runtimo_core::Telemetry;

let telemetry = Telemetry::capture();
// CPU: AMD EPYC 7B13
// RAM: 30Gi total, 13Gi free
// Disk: 148G total, 77G free (47% used)
// TPU: 0, GPU: 0
// vLLM: not installed
// Public IP: 34.45.218.104
```

**Purpose:** Know what resources exist.

### Layer 2: Process Snapshot (Persistent Pattern) ⭐

```rust
use runtimo_core::ProcessSnapshot;

let snapshot = ProcessSnapshot::capture();
// Total: 176 processes
// Top CPU: python3 (65.7%)
// Top Memory: python3 (493MB)
// Zombies: 0
// runtimo jobs: tracking...
```

**Purpose:** Know what's running, what's broken, what to kill.

## Why Process Tracking Matters for Persistent Machines

### 1. Detect Resource Hogs
```
Top CPU: python3 (65.7%)
Top Memory: python3 (493MB)
```
→ If a runtimo capability spawns a process that consumes 90% CPU, you need to know.

### 2. Zombie Detection
```
Zombies: 0
```
→ If this was 5+, something is broken. Persistent machines accumulate zombies.

### 3. Process Lineage
```
Before job: 175 processes
After job: 176 processes
Spawned: PID 12345 (python3 -c "...")
```
→ Which capability spawned this? Should it be killed?

### 4. Graceful Degradation
Ephemeral: Platform enforces limits (hard stop).  
Persistent: **Soft limits** - warn, then degrade gracefully.

```rust
if snapshot.summary.total_cpu_percent > 90.0 {
    // Don't hard-fail, just queue or degrade
    return Err(Error::ResourceLimitExceeded("System overloaded".into()));
}
```

### 5. Audit Trail
Every job execution logs:
- Before: process snapshot
- After: process snapshot
- Spawned: list of new PIDs
- Resources: CPU/RAM consumed

For persistent machines, this is **critical for forensics**.

## Implementation

### Files
- `core/src/telemetry.rs` - Hardware telemetry module
- `core/src/processes.rs` - Process snapshot module
- `core/src/executor.rs` - Integration with execution pipeline

### Commands
```bash
# Hardware telemetry (ephemeral pattern)
./target/debug/moe telemetry

# Process snapshot (persistent pattern) ⭐ NEW
./target/debug/moe processes
```

### Tests
```bash
cargo test -p runtimo-core
# test telemetry::tests::test_telemetry_capture ... ok
# test processes::tests::test_process_snapshot ... ok
```

## What This Enables

### 1. Resource-Aware Execution
```rust
let snapshot = ProcessSnapshot::capture();
if snapshot.summary.total_mem_percent > 90.0 {
    // Don't run heavy capability
    return Err(Error::ResourceLimitExceeded("Low memory".into()));
}
```

### 2. Process Tracking
```rust
let before = ProcessSnapshot::capture();
// Execute capability
let after = ProcessSnapshot::capture();
let spawned = identify_spawned(&before, &after);
// spawned = [PID 12345, PID 12346]
```

### 3. Zombie Detection
```rust
let snapshot = ProcessSnapshot::capture();
if snapshot.summary.zombie_count > 10 {
    alert_operator("High zombie count - system may be unstable");
}
```

### 4. Forensic Analysis
```rust
// Every job logs before/after snapshots
// Can reconstruct what happened and when
let history = wal_reader.query("job_completed")?;
for event in history {
    println!("Job {} used {} CPU, {} RAM", 
        event.job_id,
        event.process_after.total_cpu_percent - event.process_before.total_cpu_percent,
        // ...
    );
}
```

## Design Principles

### 1. Observation Before Action
- Always capture state before making changes
- Telemetry is **observational** - records, doesn't block
- Enforcement happens via guards (llmosafe)

### 2. Two-Layer Awareness
- Hardware: What exists (static)
- Processes: What's running (dynamic)
- Both captured before/after every execution

### 3. Forensic Readiness
- Every execution leaves a trace
- Before/after snapshots enable reconstruction
- WAL provides append-only audit trail

### 4. Graceful Degradation
- Don't hard-fail on resource pressure
- Queue, degrade, or warn
- Let operator decide

### 5. One Log Source, Not Two
- Extend existing WAL instead of creating separate logs
- Gets file locking, rotation, cleanup, seq recovery for free
- Prevents R-DRIFT (duplicate infrastructure)

### 6. Dev-Only for Telemetry, Release for Production
- Command execution logging via `#[cfg(debug_assertions)]`
- Zero overhead in release builds
- WAL events can be read in release (variant exists) but never written

### 7. Token-Efficient Error Absorption
- Per-typo correction saves ~450 tokens (no debug loop)
- Logging cost ~50 tokens/cmd
- 1KB truncation prevents WAL bloat while preserving signal

## Related Documentation

- [`Telemetry`](telemetry.md) - Hardware telemetry implementation
- [`ProcessSnapshot`](processes.md) - Process tracking implementation
- [`LlmoSafeGuard`](llmosafe.md) - Resource enforcement
- [`WalWriter`](wal.md) - Audit trail

---

**Source Files:**
- `core/src/telemetry.rs`
- `core/src/processes.rs`
- `core/src/executor.rs`
- `core/src/lib.rs`

**Verified:** 2026-05-16
