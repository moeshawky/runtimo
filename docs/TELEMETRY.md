# Telemetry Design

**Date:** 2026-05-28
**Status:** Implemented
**Source:** `core/src/telemetry.rs`

## Overview

Runtimo provides **two-layer telemetry** — hardware and process state captured before and after every capability execution.

1. **Hardware Telemetry** — CPU, RAM, disk, accelerators, services, network
2. **Process Snapshot** — Running processes with PPID tracking, zombie count, top consumers

## Design Principle: Discovery, Not Checklist

Telemetry detects what exists — it does not assume named hardware or specific services. Unavailable hardware/services are simply absent from the output. No "vLLM: not installed" or "TPU Devices: 0" noise.

## HardwareInfo Structure

```rust
// core/src/telemetry.rs
pub struct HardwareInfo {
    // Legacy fields (back-compat with old WAL data, computed from accelerators list)
    pub tpu_devices: u32,
    pub gpu_devices: u32,

    // Primary: discovery-based
    pub accelerators: Vec<AcceleratorInfo>,
}

pub struct AcceleratorInfo {
    pub kind: String,      // "nvidia", "amd", "tpu", "drm"
    pub count: u64,
    pub vendor: String,    // e.g., "NVIDIA Corporation"
    pub model: String,     // e.g., "RTX A4000"
}
```

### Accelerator Detection Order

1. **TPU** — `/dev/accel*` device files
2. **NVIDIA** — `nvidia-smi --query-gpu=name,count --format=csv,noheader`
3. **AMD** — `rocm-smi --showproductname` + `/dev/dri/render*`
4. **DRM** — `/dev/dri/render*` fallback (generic GPU)

## ServiceInfo Structure

```rust
pub struct ServiceInfo {
    // Legacy fields (back-compat)
    pub vllm_running: bool,
    pub vllm_version: String,
    pub vllm_port_bound: bool,

    // Primary: discovery-based
    pub detected_services: Vec<DetectedService>,
}

pub struct DetectedService {
    pub name: String,     // "vllm", "nginx", "postgres", "redis", "docker"
    pub version: String,
    pub running: bool,
    pub ports: Vec<u16>,
}
```

### Service Detection

Each service detected independently via `pgrep`:
- **vLLM** — `pgrep -fa vllm`, version via `python -c "import vllm"`
- **nginx** — `pgrep -fa nginx`
- **postgres** — `pgrep -fa postgres`
- **redis** — `pgrep -fa redis-server`
- **docker** — `pgrep -fa dockerd`

Only detected services are added to `detected_services`. If none are found, the list is empty — no "not running" entries.

## Process Snapshot

```rust
// core/src/processes.rs
pub struct ProcessSnapshot {
    pub processes: Vec<ProcessInfo>,  // full process list
    pub summary: ProcessSummary,      // aggregated stats
}
```

Key metrics per snapshot:
- Total process count
- Zombie count (alerts if > 10)
- Top CPU consumers (PID + command)
- Top memory consumers (PID + command + RSS)
- PPID tracking for lineage

## Verification

```bash
# Hardware telemetry
runtimo telemetry
runtimo telemetry --json

# Process snapshot
runtimo processes
runtimo processes --json

# Programmatic
cargo test -p runtimo-core --lib -- telemetry
# 8 tests: capture, cache, back-compat, empty state, serialization, old WAL deser
```

**Last Verified:** 2026-05-28 (8 telemetry tests pass, clippy clean)
