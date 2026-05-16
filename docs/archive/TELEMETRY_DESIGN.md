# Telemetry Layer Design

**Date:** 2026-05-16  
**Source:** Kaggle cell_txt.txt pattern  
**Problem:** runtimo lacked environment awareness  
**Solution:** Added `telemetry` module to `runtimo-core`

---

## The Insight

The operator said: *"the issue you're not seeing is runtimo lacks environment awareness"*

This came from analyzing `/home/moeshawky/cell_txt.txt` — a Kaggle session runner that:
1. Captures full system telemetry (CPU, RAM, disk, TPU, GPU, services, network)
2. Provides dual protocol (agent file-based + human interactive)
3. Tracks command history

runtimo had #2 (agent protocol via capabilities) but **zero** #1 (telemetry).

---

## What Telemetry Captures

| Category | Fields | Why It Matters |
|----------|--------|----------------|
| **System** | CPU model, RAM total/free, disk total/free/used%, uptime, load | Know resource availability, detect resource exhaustion |
| **Hardware** | TPU devices, GPU devices, JAX availability | Choose appropriate execution path (CPU vs TPU vs GPU) |
| **Services** | vLLM version/running/port-bound | Don't call vLLM if it's not running |
| **Network** | Public IP, tunnel status | Know if remote access is available |

---

## Implementation

**Module:** `core/src/telemetry.rs`  
**API:**
```rust
use runtimo_core::Telemetry;

// Capture snapshot
let telemetry = Telemetry::capture();

// Print human-readable report
telemetry.print_report();

// Access programmatically
if telemetry.hardware.gpu_devices > 0 {
    // Use GPU-accelerated path
}
if !telemetry.services.vllm_running {
    // Fail gracefully or start vLLM
}
```

**Verified:**
- ✅ Compiles (`cargo check` passes)
- ✅ Tests pass (`cargo test -p runtimo-core -- telemetry`)
- ✅ CLI demo works (`moe telemetry` prints report)

---

## Integration Points

### 1. WAL Logging (Future)
Every job submission/completion should include telemetry snapshot:
```json
{
  "event": "job_completed",
  "job_id": "abc123",
  "telemetry": {
    "timestamp": 1778882293,
    "system": { "ram_free": "13Gi", ... },
    "hardware": { "gpu_devices": 0, ... },
    ...
  }
}
```

### 2. Capability Guards (Future)
Capabilities can check telemetry before execution:
```rust
fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
    let telemetry = Telemetry::capture();
    
    // Guard: Don't run if RAM < 1GB
    let ram_free = parse_size(&telemetry.system.ram_free)?;
    if ram_free < 1024 * 1024 * 1024 {
        return Err(Error::ResourceLimitExceeded("RAM < 1GB".into()));
    }
    
    // Execute...
}
```

### 3. Health Checks (Future)
Daemon can monitor telemetry and alert on thresholds:
```rust
loop {
    let telemetry = Telemetry::capture();
    let disk_used = parse_percent(&telemetry.system.disk_used_percent)?;
    
    if disk_used > 90 {
        alert_operator("Disk usage > 90%!");
    }
    
    sleep(Duration::from_secs(60));
}
```

---

## Relationship to moegraph + llmosafe

**llmosafe** = Resource limits (CPU time, memory, disk I/O, timeout)  
**moegraph** = Code intelligence (graph analysis, vector search)  
**telemetry** = Environment awareness (what's available, what's running)

### How They Work Together

1. **llmosafe** enforces limits ("don't use more than 2GB RAM")
2. **telemetry** reports current state ("13Gi RAM free")
3. **moegraph** analyzes code ("this function has 100 callers")

runtimo uses all three:
- Check telemetry → Is GPU available?
- Check llmosafe → Am I within RAM limits?
- (Optional) Use moegraph → Will this break callers?

---

## Options for moegraph + runtimo Integration

As discussed earlier, three options remain:

**Option A: Complete Fusion**  
Merge moegraph + runtimo into one repo.  
→ Best if runtimo is moegraph-specific runtime.

**Option B: Partial Dependence** (Recommended)  
runtimo has `features = ["moegraph-integration"]` (optional).  
→ Best if runtimo capabilities need graph analysis.

**Option C: Orthogonal + Shared Safety**  
No direct dependency, both use llmosafe.  
→ Best if they're separate concerns.

**Telemetry doesn't change this decision** — it's a separate layer that both can use.

---

## Next Steps

1. **Integrate telemetry into WAL** (every event includes telemetry snapshot)
2. **Add telemetry guards to capabilities** (check resources before execution)
3. **Implement health monitoring** (alert on thresholds)
4. **Operator decision on moegraph integration** (fusion vs dependence vs orthogonal)

---

## Verification

```bash
# Build runtimo
cd /workspace/runtimo && cargo build

# Print telemetry
./target/debug/moe telemetry

# Run tests
cargo test -p runtimo-core -- telemetry
```

**Last Verified:** 2026-05-16 (telemetry module compiles and prints report)
