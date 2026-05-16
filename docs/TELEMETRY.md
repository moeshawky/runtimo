# Telemetry Design

**Date:** 2026-05-16  
**Status:** Implemented  
**Module:** `core/src/telemetry.rs`

## Overview

Runtimo provides **two-layer telemetry** for persistent machine awareness:
1. **Hardware Telemetry** - CPU, RAM, disk, TPU/GPU, services, network
2. **Process Snapshot** - Running processes with PPID tracking

This design captures the full environment state before and after every capability execution.

## Problem Statement

Agent capability runtimes lack environment awareness. Without telemetry:
- Agents cannot detect resource exhaustion
- Agents call services that aren't running
- Agents choose wrong execution path (CPU vs GPU vs TPU)
- No baseline for crash recovery

## Solution

`Telemetry::capture()` provides a point-in-time snapshot of:

### System Layer
| Field | Type | Description |
|-------|------|-------------|
| `cpu_model` | String | CPU identifier (e.g., "AMD EPYC 7B13") |
| `ram_total_gb` | f64 | Total RAM in GB |
| `ram_free_gb` | f64 | Free RAM in GB |
| `disk_total_gb` | f64 | Total disk space in GB |
| `disk_free_gb` | f64 | Free disk space in GB |
| `disk_used_percent` | f64 | Disk usage percentage |
| `uptime_secs` | i64 | System uptime in seconds |
| `load_avg` | [f64; 3] | Load average (1m, 5m, 15m) |

### Hardware Layer
| Field | Type | Description |
|-------|------|-------------|
| `tpu_count` | u32 | Number of TPU devices |
| `gpu_count` | u32 | Number of GPU devices |
| `jax_available` | bool | JAX library available |

### Services Layer
| Field | Type | Description |
|-------|------|-------------|
| `vllm_version` | Option<String> | vLLM version if installed |
| `vllm_running` | bool | vLLM service running |
| `port_8200_bound` | bool | Port 8200 bound |

### Network Layer
| Field | Type | Description |
|-------|------|-------------|
| `public_ip` | Option<String> | Public IP address |
| `tunnel_running` | bool | Cloudflared tunnel running |

## API Usage

### Basic Capture
```rust
use runtimo_core::Telemetry;

let telemetry = Telemetry::capture();
println!("CPU: {}", telemetry.cpu_model);
println!("RAM: {}GB free", telemetry.ram_free_gb);
println!("Disk: {}% used", telemetry.disk_used_percent);
```

### Human-Readable Report
```rust
use runtimo_core::Telemetry;

let telemetry = Telemetry::capture();
telemetry.print_report();
```

Output:
```
============================================================ RUNTIMO TELEMETRY [1778925313] ============================================================
--- SYSTEM ---
CPU : AMD EPYC 7B13
RAM : 30Gi total, 4.7Gi free
Disk : 148G total, 40G free (73% used)
Uptime: up 1 day, 2 hours
Load : 0.67, 0.47, 0.49

--- HARDWARE ---
TPU Devices: 0
GPU Devices: 0
JAX: Not available

--- SERVICES ---
vLLM: not installed
Port 8200: NOT BOUND

--- NETWORK ---
Public IP: 34.45.218.104
Tunnel: running
```

### Programmatic Checks
```rust
use runtimo_core::Telemetry;

let telemetry = Telemetry::capture();

// Check if GPU execution is possible
if telemetry.gpu_count > 0 {
    // Use GPU-accelerated path
} else {
    // Fall back to CPU
}

// Guard: Don't run if RAM < 1GB
if telemetry.ram_free_gb < 1.0 {
    return Err(Error::ResourceLimitExceeded("RAM < 1GB".into()));
}

// Guard: Check if required service is running
if !telemetry.vllm_running {
    return Err(Error::ServiceNotAvailable("vLLM not running".into()));
}
```

## Integration Points

### 1. WAL Logging
Every job submission/completion includes telemetry snapshots:
```json
{
  "event": "job_completed",
  "job_id": "abc123",
  "capability": "FileRead",
  "telemetry_before": {
    "timestamp": 1778882293,
    "system": { "ram_free": "13Gi", ... },
    "hardware": { "gpu_devices": 0, ... }
  },
  "telemetry_after": { ... }
}
```

### 2. Capability Guards
Capabilities check telemetry before execution:
```rust
fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
    let telemetry = Telemetry::capture();
    
    // Guard: Don't run if RAM < 1GB
    let ram_free = telemetry.ram_free_gb;
    if ram_free < 1.0 {
        return Err(Error::ResourceLimitExceeded("RAM < 1GB".into()));
    }
    
    // Execute capability...
}
```

### 3. Health Monitoring
Daemon monitors telemetry and alerts on thresholds:
```rust
loop {
    let telemetry = Telemetry::capture();
    
    if telemetry.ram_free_gb < 0.5 {
        alert_operator("Critical: RAM < 500MB");
    }
    
    if telemetry.disk_used_percent > 90.0 {
        alert_operator("Warning: Disk > 90% full");
    }
    
    sleep(Duration::from_secs(60));
}
```

## Implementation Details

### Capture Method
```rust
impl Telemetry {
    pub fn capture() -> Self {
        Telemetry {
            timestamp: timestamp() as i64,
            cpu_model: read_cpu_model(),
            ram_total_gb: read_meminfo("/proc/meminfo", "MemTotal"),
            ram_free_gb: read_meminfo("/proc/meminfo", "MemAvailable"),
            disk_total_gb: read_disk_stats("/"),
            disk_free_gb: read_disk_stats("/"),
            disk_used_percent: calculate_percent(),
            uptime_secs: read_uptime(),
            load_avg: read_loadavg(),
            tpu_count: count_devices("tpu"),
            gpu_count: count_devices("gpu"),
            jax_available: check_jax(),
            vllm_version: check_vllm_version(),
            vllm_running: check_service("vllm"),
            port_8200_bound: check_port(8200),
            public_ip: fetch_public_ip(),
            tunnel_running: check_tunnel(),
        }
    }
}
```

### Data Sources
| Source | Method |
|--------|--------|
| `/proc/cpuinfo` | CPU model |
| `/proc/meminfo` | RAM total/free |
| `/proc/uptime` | Uptime |
| `/proc/loadavg` | Load average |
| `df` | Disk usage |
| `lscpu` | CPU details |
| `nvidia-smi` | GPU count (if available) |
| `ls /sys/class/tpu` | TPU count |
| `ip addr` | Network interfaces |
| `curl ifconfig.me` | Public IP |

## Testing

### Unit Tests
```rust
#[test]
fn test_telemetry_capture() {
    let telemetry = Telemetry::capture();
    
    assert!(!telemetry.cpu_model.is_empty());
    assert!(telemetry.ram_total_gb > 0.0);
    assert!(telemetry.ram_free_gb >= 0.0);
    assert!(telemetry.disk_total_gb > 0.0);
    assert!(telemetry.disk_used_percent >= 0.0);
    assert!(telemetry.disk_used_percent <= 100.0);
}
```

### CLI Verification
```bash
./target/debug/moe telemetry
```

Expected output: Full system report as shown above.

## Performance Characteristics

| Metric | Value |
|--------|-------|
| Capture time | <100ms |
| Memory usage | <1MB |
| Allocations | ~20 |
| I/O calls | ~15 (procfs, sysfs, network) |

## Future Enhancements

### 1. Historical Tracking
```rust
let history = TelemetryHistory::load();
let trend = history.ram_trend(60); // Last 60 seconds
if trend.is_increasing_rapidly() {
    alert_operator("RAM leak detected");
}
```

### 2. Predictive Alerts
```rust
let prediction = telemetry.predict_disk_full();
if prediction.days_until_full < 7 {
    alert_operator("Disk will be full in < 7 days");
}
```

### 3. Process Correlation
```rust
let (telemetry, processes) = Telemetry::capture_with_processes();
let top_consumer = processes.top_by_memory();
println!("{} using {}% RAM", top_consumer.comm, top_consumer.mem_percent);
```

## Related Documentation

- [`ProcessSnapshot`](processes.md) - Process tracking
- [`LlmoSafeGuard`](llmosafe.md) - Resource guards
- [`WalWriter`](wal.md) - WAL logging

---

**Source Files:**
- `core/src/telemetry.rs` - Implementation
- `core/src/telemetry.rs` - Tests
- `cli/src/main.rs` - CLI integration

**Verified:** 2026-05-16
