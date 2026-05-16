# Telemetry Layer — Summary

**Date:** 2026-05-16  
**Source:** Kaggle cell_txt.txt pattern (`/home/moeshawky/cell_txt.txt`)  
**Status:** ✅ Implemented, compiled, tested

---

## Before (runtimo without telemetry)

```
Agent: "Execute FileRead on /path/to/file"
runtimo: "OK, executing..."
[No idea if RAM is low, disk is full, or required service is down]
```

**Problem:** runtimo executed capabilities blindly — no environment awareness.

---

## After (runtimo with telemetry)

```
Agent: "Execute FileRead on /path/to/file"
runtimo: "Capturing telemetry..."
  - RAM: 13Gi free (OK)
  - Disk: 77G free (OK)
  - vLLM: not running (not needed for FileRead)
  - Tunnel: running (can report results)
runtimo: "Environment OK, executing..."
```

**Solution:** runtimo now captures full system telemetry before/during/after execution.

---

## What Changed

| Component | Before | After |
|-----------|--------|-------|
| **Module** | None | `core/src/telemetry.rs` (200+ lines) |
| **CLI** | No telemetry command | `moe telemetry` prints full report |
| **API** | No telemetry access | `Telemetry::capture()` returns structured data |
| **Tests** | None | `test_telemetry_capture()` passes |
| **Docs** | None | `TELEMETRY_DESIGN.md`, `ARCHITECTURE.md` |

---

## Telemetry Output (Live Example)

```
============================================================
 RUNTIMO TELEMETRY [1778882293]
============================================================
--- SYSTEM ---
 CPU   : AMD EPYC 7B13
 RAM   : 30Gi total, 13Gi free
 Disk  : 148G total, 77G free (47% used)
 Uptime: up 14 hours, 12 minutes
 Load  : 1.27, 1.20, 1.09
--- HARDWARE ---
 TPU Devices: 0
 GPU Devices: 0
 JAX: Not available
--- SERVICES ---
 vLLM: not installed
 Port 8200: NOT BOUND
--- NETWORK ---
 Public IP: 34.45.218.104
 Tunnel: running (cloudflared)
============================================================
```

---

## Why This Matters

### 1. Resource-Aware Execution
```rust
let telemetry = Telemetry::capture();
if telemetry.system.ram_free.parse::<Size>()? < threshold {
    return Err(Error::ResourceLimitExceeded("Low RAM".into()));
}
```

### 2. Service Dependency Checks
```rust
if !telemetry.services.vllm_running {
    // Don't try to call vLLM — it's not running!
    return Err(Error::ExecutionFailed("vLLM not running".into()));
}
```

### 3. Audit Trail
Every job now includes telemetry snapshot:
```json
{
  "job_id": "abc123",
  "capability": "FileRead",
  "telemetry": {
    "timestamp": 1778882293,
    "system": { "ram_free": "13Gi", ... },
    ...
  }
}
```

### 4. Health Monitoring
Daemon can alert on thresholds:
```rust
if parse_percent(&telemetry.system.disk_used_percent)? > 90 {
    alert_operator("Disk usage critical!");
}
```

---

## Files Changed

| File | Change |
|------|--------|
| `core/src/telemetry.rs` | **NEW** (200+ lines, telemetry capture + reporting) |
| `core/src/lib.rs` | Added `pub mod telemetry; pub use telemetry::Telemetry;` |
| `core/Cargo.toml` | Added `time = "0.3"` dependency |
| `cli/src/main.rs` | Added `moe telemetry` command |
| `docs/ARCHITECTURE.md` | **NEW** (full architecture with telemetry layer) |
| `TELEMETRY_DESIGN.md` | **NEW** (design doc) |
| `TELEMETRY_SUMMARY.md` | **NEW** (this file) |
| `AGENTS.md` | Added telemetry section |
| `README.md` | Added telemetry section |

---

## Verification

```bash
# Build
cd /workspace/runtimo && cargo build

# Test
cargo test -p runtimo-core -- telemetry

# Run telemetry command
./target/debug/moe telemetry
```

All verified ✅

---

##
## Next: moegraph + runtimo Integration

The original question remains: **fusion or partial dependence?**

Now with telemetry, runtimo can:
- Check environment before running moegraph capabilities
- Log moegraph usage in telemetry (which graph algorithms ran)
- Monitor moegraph service health (is the graph DB running?)

**Recommendation unchanged:** Option B (optional dependency)
- runtimo has `features = ["moegraph-integration"]`
- Default: pure runtime (no moegraph)
- With feature: code-aware capabilities (UpdateFunction, Refactor)

**Your move, operator.** Telemetry is done. What's next?

---

## Process Execution Layer (Persistent Machines)

**Added:** `core/src/processes.rs` - `ps aux` style process awareness

```bash
# Print process snapshot
./target/debug/moe processes

# Example output:
# ================================================================================
#  PROCESS SNAPSHOT [1778882991]
# ================================================================================
# --- SUMMARY ---
#  Total Processes: 176
#  Total CPU: 195.6%
#  Total Memory: 4.7%
#  Zombies: 0
#  Top CPU: ps aux (100.0%)
#  Top Memory: opencode --continue (1.5%)
# --- TOP 10 BY CPU ---
#   1. 191742 moeshaw+ 100.0 0.0 8.5G 4.6G "R" ps aux
#   2. 181354 moeshaw+ 65.7 0.5 176.8G 162.5G "R" /home/moeshawky/.pyenv/...
#   ...
# --- TOP 10 BY MEMORY ---
#   1. 80605 moeshaw+ 10.0 1.5 73348.6G 493.4G "Sl+" opencode --continue
#   ...
```

**Why This Matters for Persistent Machines:**

| Ephemeral (Kaggle) | Persistent (Your Machine) |
|-------------------|--------------------------|
| Factory reset anytime | **Can't factory reset - data!** |
| No process tracking needed | **Track what's running** |
| Session ends = gone forever | **Session ends = processes may linger** |
| Resource limits = hard stop | **Resource limits = warn + graceful degradation** |

**runtimo now knows:**
- What processes are running (and which spawned them)
- Which processes are CPU hogs
- Which processes are memory hogs
- Zombie processes (broken jobs)
- Can kill runaway capabilities by PID

**Next:** Integrate process tracking into job execution (track child PIDs, detect runaways).
