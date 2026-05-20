# Runtimo Architecture

**Version:** 0.2.1  
**Last Updated:** 2026-05-20  
**Inspired by:** Kaggle session telemetry pattern (cell_txt.txt)

---

## The Problem: Environment Amnesia

runtimo's original design could execute capabilities but had **zero environment awareness**:
- No idea what resources are available (CPU, RAM, disk, TPU, GPU)
- No idea what's running (processes, services like vLLM)
- No idea about network state (public IP, tunnel status)

This is like a self-driving car that can steer but can't see the road.

**Source:** Kaggle cell_txt.txt pattern — captures full system telemetry before executing agent commands.

---

## The Solution: Telemetry-Aware Runtime

runtimo now has **environment awareness** as a core capability:

```rust
use runtimo_core::telemetry::Telemetry;

// Capture full system telemetry
let telemetry = Telemetry::capture();

// Print human-readable report
telemetry.print_report();

// Or access programmatically
if telemetry.hardware.tpu_devices > 0 {
    // Use TPU-accelerated capability
}
if !telemetry.services.vllm_running {
    // Start vLLM or fail gracefully
}
```

---

## Full Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    CLI / Agent Interface                        │
│  (moe run/status/undo/logs, JSON-RPC, Unix socket, HTTP?)      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                   API Layer (Daemon)                             │
│  - Unix socket listener (JSON-RPC protocol)                     │
│  - Job queue (in-memory, persisted to WAL)                      │
│  - Telemetry integration (environment-aware execution)          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              Validation Layer                                   │
│  - Capability schema validation (JSON Schema)                   │
│  - Permission checks                                            │
│  - Telemetry-based guards (e.g., "don't run if RAM < 1GB")      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              LLMOSafe Layer (Resource Limits)                   │
│  - CPU time limits                                              │
│  - Memory limits                                                │
│  - Disk I/O limits                                              │
│  - Timeout (wall-clock)                                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│          Execution Layer (Capabilities)                         │
│  - FileRead, FileWrite, FileExists                              │
│  - ShellExec (sandboxed)                                        │
│  - [Future] Code-aware: UpdateFunction, Refactor (via moegraph) │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│           Logging Layer (WAL - Write-Ahead Log)                 │
│  - Append-only JSONL                                            │
│  - fsync after each event                                       │
│  - Includes telemetry snapshot per job                          │
│  - CommandExecuted events (dev-only, `#[cfg(debug_assertions)]`) │
│    record cmd, stdout/stderr (1KB trunc), exit code, correction │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│          Recovery Layer (Backup + Undo)                         │
│  - Backup before mutations                                      │
│  - Restore on undo                                              │
│  - Telemetry-aware rollback (restore to known-good state)       │
└─────────────────────────────────────────────────────────────────┘
```

---

## Telemetry Module (`core/src/telemetry.rs`)

### Structure

```rust
pub struct Telemetry {
    pub timestamp: u64,
    pub system: SystemInfo,      // CPU, RAM, disk, uptime, load
    pub hardware: HardwareInfo,  // TPU, GPU, JAX
    pub services: ServiceInfo,   // vLLM version/running/port
    pub network: NetworkInfo,    // Public IP, tunnel status
}
```

### What It Captures

| Category | Fields | Example Values |
|----------|--------|----------------|
| **System** | cpu_model, ram_total/free, disk_total/free/used%, uptime, load | "Intel Xeon", "16G total, 8G free", "256G total, 100G free (61% used)" |
| **Hardware** | tpu_devices, gpu_devices, jax_available/version/device_count | 2 TPU chips, 4 GPU devices, JAX v0.4.26 (8 devices) |
| **Services** | vllm_version, vllm_running, vllm_port_bound | v0.3.0, running=true, port_bound=true |
| **Network** | public_ip, tunnel_running, tunnel_name | "123.45.67.89", running=true, "cloudflared tunnel abc123" |

### Usage in Capabilities

```rust
// In a capability's execute() method:
fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
    // Capture telemetry before execution
    let telemetry = Telemetry::capture();
    
    // Check resource availability
    if telemetry.hardware.gpu_devices == 0 {
        return Err(Error::ExecutionFailed("GPU required but not available".into()));
    }
    
    // Check service availability
    if !telemetry.services.vllm_running {
        return Err(Error::ExecutionFailed("vLLM service not running".into()));
    }
    
    // Execute with confidence
    // ...
    
    // Log telemetry with job for audit trail
    Ok(Output { success: true, data: json!({}), message: None })
}
```

---

## Relationship to moegraph

**moegraph** = Code intelligence (graph analysis, vector search, MCP)  
**runtimo** = Capability runtime (validation, execution, logging, undo, **telemetry**)  
**llmosafe** = Shared safety layer (both depend on this)

### Options (awaiting operator decision):

**Option A: Complete Fusion**  
Merge moegraph + runtimo into one repo.  
→ Loses standalone value, gains tight integration.

**Option B: Partial Dependence** (Recommended)  
runtimo optionally depends on moegraph (`features = ["moegraph-integration"]`).  
→ runtimo stays independent, can use moegraph for code-aware capabilities.

**Option C: Orthogonal + Shared Safety**  
No direct dependency, both use llmosafe.  
→ Clean boundaries, no cross-capabilities.

---

## Verification Commands

```bash
# Verify telemetry module compiles
cd /workspace/runtimo && cargo check

# Capture and print telemetry (when CLI supports it)
moe telemetry

# Or programmatically
cargo run --bin runtimo --features telemetry
```

---

**Sources:**
- Kaggle cell_txt.txt pattern (telemetry + dual protocol)
- moegraph architecture (`/workspace/moegraph/src/lib.rs`)
- llmosafe patterns (optional features)

**Last Verified:** 2026-05-16 (telemetry module compiles)  
**Next Review:** After first telemetry-aware capability implementation
