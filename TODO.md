# Runtimo Status

**Last Updated:** 2026-05-16  
**Build:** `cargo build --workspace` — clean, 0 warnings  
**Tests:** 69 passing (13 unit + 31 integration + 7 doc + 18 schema/backup)  
**Examples:** 3 runnable (basic_read, telemetry_demo, write_and_undo)

---

## ✅ Complete

### Core Library (`runtimo-core`)
- Capability trait + registry (name, schema, validate, execute)
- Executor pipeline: telemetry → llmosafe gate → execute → WAL
- JSON Schema validation via `jsonschema` crate
- WAL (append-only JSONL with fsync)
- Backup manager with cleanup (age-based deletion)
- llmosafe v0.6 integration (ResourceGuard, pressure, entropy)

### Capabilities
- **FileRead** — path traversal protection, exists/is_file validation
- **FileWrite** — backup-before-mutate, undo support, append mode, dry-run

### Telemetry & Process
- Hardware telemetry: CPU model, RAM, disk, TPU/GPU, services, network
- Process snapshot: ps aux parsing, zombie detection, top consumers
- Both captured before/after every execution

### CLI (`moe`)
- `run` — execute capability with telemetry + WAL
- `list` — available capabilities
- `telemetry` — hardware report
- `processes` — process snapshot
- `status` — job history from WAL
- `logs` — WAL event viewer
- `undo` — restore from backup

### Daemon (`runtimo`)
- Unix socket JSON-RPC at `/tmp/runtimo.sock`
- Methods: `run`, `list`, `logs`
- Per-client async handler, stale socket cleanup

### Documentation
- README.md (296 lines) — architecture, quickstart, CLI, safety model
- rustdoc on all public types (cargo doc — zero warnings)
- 3 runnable examples

### CI
- `.github/workflows/ci.yml` — build, test, clippy, fmt

---

## 📋 Remaining

### P0: Health Monitoring
- [ ] Background daemon with periodic snapshots (every 60s)
- [ ] Alert on zombie count > threshold
- [ ] Alert on memory leak detection (monotonic RSS increase)
- [ ] Alert on CPU hog detection

### P1: Kill Capability
- [ ] `moe kill --pid <pid>` with confirmation
- [ ] `kill_job(job_id)` — kill spawned processes
- [ ] Log kill events to WAL

### P2: WAL Telemetry Snapshots
- [ ] Include full telemetry_before/after in WAL events (currently just job metadata)
- [ ] Include process snapshots in job start/complete events

### P3: Runbooks
- [ ] "How to add a new capability"
- [ ] "How to monitor persistent machine health"
- [ ] "How to recover from runaway jobs"

### P4: Architecture Decisions
- [ ] moegraph + runtimo integration (Option A/B/C — awaiting operator)

### P5: Process Enhancements
- [ ] PPID capture (`ps -eo pid,ppid,...`)
- [ ] Process lineage tracking (parent-child)

---

## 🔮 Future (Not Planned)
- Resource usage time-series database
- ML-based anomaly detection
- Auto-kill policies
- Process tree visualization (TUI/web)
- moegraph integration (UpdateFunction, Refactor with dependency tracking)
