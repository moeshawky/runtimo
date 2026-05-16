# Continuation Guide

**For:** Next agent or operator continuing Runtimo development  
**Date:** 2026-05-16  
**Status:** Core scaffold complete, telemetry + process awareness implemented

---

## Quick Start

```bash
cd /workspace/runtimo

# Build
cargo build

# Test
cargo test -p runtimo-core

# View telemetry (hardware)
./target/debug/moe telemetry

# View processes (execution)
./target/debug/moe processes

# Check task list
cat TODO.md
```

---

## What's Done

### Core Infrastructure ✅
- 3-crate workspace: `runtimo-core` (lib), `runtimo` (daemon), `moe` (CLI)
- Core modules: `job`, `capability`, `schema`, `wal`, `backup`, `llmosafe`, `telemetry`, `processes`
- Build verification: `cargo build` passes
- Tests: 2 passing (telemetry + process snapshot)

### Telemetry Layer ✅
- **Purpose:** Hardware awareness (ephemeral machine pattern)
- **Module:** `core/src/telemetry.rs`
- **CLI:** `moe telemetry`
- **Captures:** CPU, RAM, disk, uptime, load, TPU/GPU, services, network

### Process Execution Layer ✅
- **Purpose:** Process awareness (persistent machine pattern)
- **Module:** `core/src/processes.rs`
- **CLI:** `moe processes`
- **Captures:** All processes, CPU/mem usage, zombies, top consumers

### Documentation ✅
- `README.md` - Project overview
- `AGENTS.md` - Agent instructions (references TODO.md)
- `docs/API.md` - API documentation
- `TELEMETRY_SUMMARY.md` - Before/after comparison
- `PERSISTENT_MACHINE_DESIGN.md` - Persistent machine rationale
- `DESIGN_DECISION.md` - Full design decision
- `docs/ARCHITECTURE.md` - Architecture with both layers

---

## What's Next

**See:** [`TODO.md`](./TODO.md) for complete task list

### Immediate Next Steps (P0)
1. Implement job execution with process tracking
   - Capture process snapshot before job
   - Capture process snapshot after job
   - Log spawned processes to WAL

2. Add resource guards to capabilities
   - Check system CPU% before running
   - Check system RAM% before running

### Awaiting Operator Decision
- **moegraph + runtimo integration strategy** (see TODO.md "In Progress")
  - Option A: Complete fusion
  - Option B: Partial dependence (recommended)
  - Option C: Orthogonal

---

## Key Design Insights

### 1. Ephemeral vs Persistent Machines
- **Ephemeral (Kaggle):** Factory reset, no process tracking needed
- **Persistent (Your box):** Can't reset, must track processes and detect anomalies

### 2. Two-Layer Telemetry
- **Layer 1 (Hardware):** What exists? (CPU, RAM, disk, etc.)
- **Layer 2 (Process):** What's running? What's broken? What to kill?

### 3. Process Tracking Benefits
- Detect resource hogs
- Zombie detection
- Process lineage (what did this job spawn?)
- Audit trail for forensics

---

## File Reference

| File | Purpose |
|------|---------|
| `core/src/lib.rs` | Core library exports |
| `core/src/telemetry.rs` | Hardware telemetry module |
| `core/src/processes.rs` | Process snapshot module |
| `core/src/job.rs` | Job lifecycle (stub) |
| `core/src/capability.rs` | Capability trait (stub) |
| `core/src/wal.rs` | Write-ahead log (stub) |
| `cli/src/main.rs` | CLI with telemetry + processes commands |
| `daemon/src/main.rs` | Daemon placeholder |
| `TODO.md` | **Task list (start here)** |
| `AGENTS.md` | Agent instructions |

---

## Common Commands

```bash
# Build and test
cargo build
cargo test -p runtimo-core

# View telemetry
./target/debug/moe telemetry
./target/debug/moe processes

# Check specific task
cat TODO.md | grep "P0"

# View design docs
cat DESIGN_DECISION.md
cat PERSISTENT_MACHINE_DESIGN.md
```

---

## Operator Notes

- **Machine:** `imported-confidential` (GCP Debian 12)
- **Location:** `/workspace/runtimo`
- **Context:** Designed for persistent machines (can't factory reset)
- **Inspired by:** Kaggle `cell_txt.txt` pattern (ephemeral telemetry) + `ps aux` (persistent process tracking)

---

**Last Updated:** 2026-05-16  
**Next Review:** After P0 (job execution integration) completion
