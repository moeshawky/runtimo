# Runtimo Status

**Last Updated:** 2026-05-20  
**Build:** `cargo clippy --workspace` — clean, 0 warnings  
**Tests:** 108 lib + 31 integration + 31 robust + 6 doc = 176 total (all passing)  
**Version:** 0.2.1

---

## ✅ Complete

### Core Library (`runtimo-core`)
- Capability trait + registry (name, schema, validate, execute)
- Executor pipeline: telemetry → llmosafe gate → execute → WAL
- JSON Schema validation via `jsonschema` crate
- WAL (append-only JSONL with fsync, rotation, cleanup, file locking)
- Backup manager with cleanup (age-based deletion, integrity verification)
- llmosafe v0.6 integration (ResourceGuard, pressure, entropy)
- Config file (`~/.config/runtimo/config.toml`) with env var override
- Session tracking with persistence and resume
- Health monitor (background snapshots every 60s, CPU/RAM alerts)
- Process tracking with lineage (PPID, descendants via /proc/children)
- Error-absorbing command logging via WAL (Phase 1, dev-only)

### Capabilities
- **FileRead** — path traversal protection, O_NOFOLLOW, binary detection, UTF-8 safe truncation, JSON auto-parse
- **FileWrite** — backup-before-mutate, undo support, append mode, dry-run, atomic write, critical file denylist
- **ShellExec** — `sh -c` execution, timeout enforcement, descendant kill, dangerous command blocklist, WAL audit
- **Kill** — POSIX signal support, protected PID list, PID reuse prevention, exponential backoff retry
- **GitExec** — clone/pull/commit/revert/clean/status, URL sanitization, secret file detection, SSRF protection
- **Undo** — restore from backup via job ID

### Telemetry & Process
- Hardware telemetry: CPU model, RAM, disk, TPU/GPU, services, network
- Process snapshot: ps aux parsing, zombie detection, top consumers, PPID tracking
- Both captured before/after every execution

### CLI (`moe`)
- `run` — execute capability with telemetry + WAL
- `list` — available capabilities with descriptions
- `telemetry` — hardware report
- `processes` — process snapshot
- `status` — job history from WAL
- `logs` — WAL event viewer (filterable by job ID)
- `undo` — restore from backup
- `config` — manage allowed paths
- `session` — create/list/resume sessions
- Compiler-error style help (req/opt/blk/ex labels)

### Daemon (`runtimo`)
- Unix socket JSON-RPC at `/tmp/runtimo.sock`
- Methods: `run`, `list`, `logs`
- Per-client async handler, stale socket cleanup, UID auth via SO_PEERCRED

### Documentation
- README.md — architecture, quickstart, CLI, safety model
- CHANGELOG.md — full version history
- AGENTS.md — implementation guide for AI agents
- docs/ARCHITECTURE.md, DESIGN.md, API.md, runbooks

### Testing
- 108 unit tests (lib)
- 31 integration tests
- 31 robust tests (6 G-categories: G-EDGE, G-SEC, G-ERR, G-CTX, G-SEM, G-DRIFT)
- 4 property-based tests (proptest)
- 6 doc tests
- CI: build, test, clippy, fmt

---

## 📋 Remaining

### P0: WAL CommandLogging Phase 2
- [ ] Auto-correction: detect common typos ("hed" → "head") and emit corrected command
- [ ] Pattern analysis: identify most frequent failure modes from CommandExecuted events
- [ ] Build failure-mode database for agent prompt improvement

### P1: CLI Improvements
- [ ] `-f/--args-file` flag to pass args as file (fixes JSON escaping issues)
- [ ] Configurable ShellExec timeout per-command (currently fixed at 30s default)

### P2: Capability Enhancements
- [ ] HTTP request capability (via reqwest)
- [ ] Concurrent job execution with worker pool
- [ ] Backup cleanup policy (TTL-based deletion)

### P3: Daemon
- [ ] Full JSON-RPC server implementation
- [ ] Process isolation (subprocess with cgroups/namespaces)
- [ ] True pre-emptive timeout enforcement

### P4: Monitoring
- [ ] Alert on zombie count > threshold (stub exists)
- [ ] Alert on memory leak detection (monotonic RSS increase)
- [ ] Alert on CPU hog detection
- [ ] Time-series database for resource usage

### P5: Runbooks
- [ ] "How to add a new capability"
- [ ] "How to monitor persistent machine health"
- [ ] "How to recover from runaway jobs"

---

## 🔮 Future (Not Planned)
- ML-based anomaly detection
- Auto-kill policies
- Process tree visualization (TUI/web)
- moegraph integration (UpdateFunction, Refactor with dependency tracking)
- Prometheus metrics export
- Distributed tracing (OpenTelemetry)
