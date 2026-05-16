# Design Decision: Persistent Machine Awareness

**Date:** 2026-05-16  
**Operator Insight:** "telemetry was designed for ephemeral machines... this is for persistent machines"

---

## The Problem

I (the AI) initially implemented telemetry based on the Kaggle `cell_txt.txt` pattern:
- CPU model, RAM, disk, uptime
- Hardware: TPU, GPU, JAX
- Services: vLLM, tunnels
- Network: public IP

**But this is wrong for persistent machines.**

### Ephemeral Machines (Kaggle, Colab)
- Factory reset with a button press
- Session ends = everything vanishes
- Telemetry = "What hardware exists?" (static snapshot)
- No need to track processes (they all die on reset)

### Persistent Machines (Your dev box, servers)
- **Cannot factory reset** - data is at stake
- Sessions end, but processes may linger
- Telemetry must answer: **"What's running? What's broken? What can I kill?"**
- Process tracking is **critical** (not optional)

---

## The Solution: Two-Layer Telemetry

runtimo now has **both** layers:

### Layer 1: Hardware Telemetry (Ephemeral Pattern)
```rust
let telemetry = Telemetry::capture();
// CPU: AMD EPYC 7B13
// RAM: 30Gi total, 13Gi free
// Disk: 148G total, 77G free (47% used)
// TPU: 0, GPU: 0
// vLLM: not installed
// Public IP: 34.45.218.104
```

**Purpose:** Know what resources exist.

### Layer 2: Process Execution (Persistent Pattern) ⭐ NEW
```rust
let snapshot = ProcessSnapshot::capture();
// Total: 176 processes
// Top CPU: opencode (65.7%)
// Top Memory: opencode (493MB)
// Zombies: 0
// runtimo jobs: tracking...
```

**Purpose:** Know what's running, what's broken, what to kill.

---

## Why Process Tracking Matters for Persistent Machines

### 1. Detect Resource Hogs
```
Top CPU: opencode (65.7%)
Top Memory: opencode (493MB)
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
After job:  176 processes
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

---

## Implementation

### Files Added
- `core/src/processes.rs` - Process snapshot module (ps aux style)
- `core/src/telemetry.rs` - Hardware telemetry module (Kaggle pattern)

### Files Modified
- `core/src/lib.rs` - Exports both modules
- `cli/src/main.rs` - Added `moe processes` command
- `core/Cargo.toml` - Added `time` dependency

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

---

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
run_capability(args)?;
let after = ProcessSnapshot::capture();

// Log spawned processes
let spawned = after.processes.difference(&before.processes);
wal.log(WalEvent::JobSpawned { pids: spawned });
```

### 3. Health Monitoring
```rust
loop {
    let snapshot = ProcessSnapshot::capture();
    if snapshot.summary.zombie_count > 5 {
        alert_operator("Multiple zombies detected!");
    }
    sleep(Duration::from_secs(60));
}
```

### 4. Kill Runaways
```rust
// Future capability
fn kill_job(job_id: &str) {
    let snapshot = ProcessSnapshot::capture();
    let job_proc = snapshot.find_by_pattern(&format!("runtimo-job-{}", job_id));
    if let Some(proc) = job_proc.first() {
        kill(proc.pid)?;
    }
}
```

---

## Relationship to moegraph + llmosafe

| System | Purpose | Persistent Machine Role |
|--------|---------|------------------------|
| **llmosafe** | Resource limits (CPU time, memory, timeout) | Enforces hard limits |
| **moegraph** | Code intelligence (graph analysis) | Optional code-aware capabilities |
| **runtimo telemetry** | Hardware awareness | Know what exists |
| **runtimo processes** ⭐ | Process awareness | Know what's running, what to kill |

**Integration:** runtimo uses llmosafe for hard limits, telemetry for environment awareness, processes for execution tracking, and optionally moegraph for code-aware capabilities.

---

## Operator Decision: moegraph + runtimo

The original question remains: **fusion, partial dependence, or orthogonal?**

With process tracking, runtimo can:
- Track which moegraph processes are running
- Detect if moegraph graph DB is consuming too much RAM
- Kill runaway moegraph queries

**Recommendation unchanged:** Option B (optional dependency)
- runtimo has `features = ["moegraph-integration"]`
- Default: pure runtime (no moegraph)
- With feature: code-aware capabilities + process tracking

---

## Verification

```bash
# Build
cd /workspace/runtimo && cargo build

# Test telemetry (ephemeral pattern)
./target/debug/moe telemetry

# Test processes (persistent pattern) ⭐
./target/debug/moe processes

# Run all tests
cargo test -p runtimo-core
```

All verified ✅

---

**Sources:**
- Operator insight: "telemetry was designed for ephemeral machines... this is for persistent machines"
- Kaggle cell_txt.txt pattern (ephemeral telemetry)
- `ps aux`, `top`, `htop` patterns (persistent process awareness)

**Last Verified:** 2026-05-16 (both telemetry and process modules compile and pass tests)
