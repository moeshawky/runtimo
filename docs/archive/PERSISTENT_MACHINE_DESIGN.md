# Persistent Machine Design

**Date:** 2026-05-16  
**Source:** Operator insight: "telemetry was designed for ephemeral machines... this is for persistent machines"  
**Problem:** Kaggle-style telemetry (CPU/RAM/disk snapshots) is for machines you can factory reset. Persistent machines need **process execution awareness**.

---

## The Core Insight

### Ephemeral Machines (Kaggle, Colab, etc.)
- **Lifecycle:** Create → Run → Reset (vanishes)
- **Telemetry:** "What hardware exists?" (static)
- **Failure mode:** Session dies, lose everything
- **Recovery:** Factory reset (clean slate)
- **Process tracking:** Not needed (everything dies on reset)

### Persistent Machines (Your dev box, servers)
- **Lifecycle:** Create → Run → Run → Run (years)
- **Telemetry:** "What's running? What's broken? What can I kill?" (dynamic)
- **Failure mode:** Resource exhaustion, zombie processes, memory leaks
- **Recovery:** Kill specific processes, restore from backup
- **Process tracking:** **Critical** (must know what to kill)

---

## What Persistent Machines Need

### 1. Process Execution Awareness (`core/src/processes.rs`)

```rust
use runtimo_core::ProcessSnapshot;

// Capture all running processes
let snapshot = ProcessSnapshot::capture();

// Print ps aux style report
snapshot.print_report();

// Find runtimo-spawned processes
let my_jobs = snapshot.find_by_pattern("runtimo-job-");

// Detect resource hogs
let top_cpu = snapshot.top_by_cpu(1);
if top_cpu.cpu_percent > 90.0 {
    // Alert: something is consuming all CPU!
}

// Kill runaway jobs
for proc in my_jobs {
    if proc.elapsed.parse::<Duration>()? > timeout {
        kill(proc.pid)?;
    }
}
```

### 2. What It Shows (Live from Your Machine)

| Metric | Value | Why It Matters |
|--------|-------|----------------|
| **Total Processes** | 176 | Baseline for anomaly detection |
| **Top CPU** | opencode (65.7%) | LLM agent runner - expected |
| **Top Memory** | opencode (493MB) | Within limits |
| **Zombies** | 0 | Healthy (no broken jobs) |
| **rclone mount** | 262MB virtual | GDrive mount - normal |
| **Xorg** | 81MB | XRDP session - normal |

### 3. Integration Points

#### A. Job Execution Tracking
```rust
// Before running a capability
let before = ProcessSnapshot::capture();
let job_pid = run_capability(capability, args)?;

// After completion
let after = ProcessSnapshot::capture();

// Detect spawned processes
let spawned = after.processes.iter()
    .filter(|p| p.pid > before.processes.last().unwrap().pid)
    .collect();

// Log in WAL
wal.log(WalEvent {
    event: "job_completed",
    job_id,
    spawned_processes: spawned.len(),
    // ...
});
```

#### B. Resource Guards
```rust
fn execute_with_guards(&self, args: &Value) -> Result<Output> {
    let snapshot = ProcessSnapshot::capture();
    
    // Guard: Don't run if system is overloaded
    if snapshot.summary.total_cpu_percent > 90.0 {
        return Err(Error::ResourceLimitExceeded(
            "System CPU > 90%".into()
        ));
    }
    
    // Guard: Don't run if memory is critical
    if snapshot.summary.total_mem_percent > 90.0 {
        return Err(Error::ResourceLimitExceeded(
            "System RAM > 90%".into()
        ));
    }
    
    // Execute capability
    // ...
}
```

#### C. Health Monitoring
```rust
loop {
    let snapshot = ProcessSnapshot::capture();
    
    // Detect zombies
    if snapshot.summary.zombie_count > 5 {
        alert_operator("Multiple zombie processes detected!");
    }
    
    // Detect memory leaks (monotonic increase over time)
    if is_memory_leak(&snapshot) {
        alert_operator("Possible memory leak detected!");
    }
    
    sleep(Duration::from_secs(60));
}
```

---

## Architecture: Ephemeral + Persistent

runtimo now has **both** layers:

```
┌─────────────────────────────────────────────────────────┐
│           Telemetry Layer (Ephemeral Focus)             │
│  - CPU model, RAM, disk, uptime                         │
│  - Hardware: TPU, GPU, JAX                              │
│  - Services: vLLM, tunnels                              │
│  - Network: public IP                                   │
│  "What exists?"                                         │
└─────────────────────────────────────────────────────────┘
                          ▼
┌─────────────────────────────────────────────────────────┐
│        Process Layer (Persistent Focus)                 │
│  - All running processes (ps aux)                       │
│  - Resource consumption per process                     │
│  - Zombie detection                                     │
│  - Parent-child relationships                           │
│  "What's running and what's broken?"                    │
└─────────────────────────────────────────────────────────┘
                          ▼
┌─────────────────────────────────────────────────────────┐
│         Capability Execution Layer                       │
│  - Run FileRead, FileWrite, etc.                        │
│  - Track spawned processes                              │
│  - Enforce resource limits (llmosafe)                   │
│  - Log to WAL with telemetry snapshot                   │
└─────────────────────────────────────────────────────────┘
```

---

## Why This Matters for Persistent Machines

### 1. You Can't Factory Reset
Ephemeral: Session dies, create new one.  
Persistent: **Data is at stake** - must recover gracefully.

**Solution:** Track processes, detect anomalies, kill runaways before they corrupt data.

### 2. Resource Exhaustion is Silent
Ephemeral: Resource limits enforced by platform.  
Persistent: **No hard limits** - processes can consume everything.

**Solution:** Monitor process resource usage, alert on thresholds, enforce soft limits.

### 3. Zombie Processes Accumulate
Ephemeral: Everything dies on reset.  
Persistent: **Zombies linger** - consume resources, confuse monitoring.

**Solution:** Detect zombies, alert operator, optionally clean up.

### 4. You Need to Know "What Did This Job Spawn?"
Ephemeral: Doesn't matter (session ends).  
Persistent: **Audit trail required** - what processes did capability X create?

**Solution:** Capture process snapshot before/after job execution, log spawned PIDs.

---

## Next Steps

1. **Integrate process tracking into job execution**
   - Capture snapshot before job
   - Capture snapshot after job
   - Log spawned processes to WAL

2. **Add process-based resource guards**
   - Don't run if system overloaded
   - Kill job if it spawns too many children

3. **Implement health monitoring daemon**
   - Periodic process snapshots
   - Alert on anomalies (zombies, memory leaks, CPU hogs)

4. **Add process kill capability**
   - `moe kill --pid 12345`
   - With confirmation for safety

---

## Verification

```bash
# Build
cd /workspace/runtimo && cargo build

# Print process snapshot
./target/debug/moe processes

# Run tests
cargo test -p runtimo-core -- processes
```

All verified ✅

---

**Sources:**
- Operator insight: "telemetry was designed for ephemeral machines... this is for persistent machines"
- Kaggle cell_txt.txt pattern (ephemeral telemetry)
- `ps aux`, `top`, `htop` patterns (persistent process awareness)

**Last Verified:** 2026-05-16 (process snapshot compiles and prints report)
