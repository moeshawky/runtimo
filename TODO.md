# Runtimo Status

**Last Updated:** 2026-05-28
**Build:** `cargo clippy --all-targets` — clean, 0 warnings
**Tests:** 120 lib + 24 doc + 31 integration + 31 robust = 206 total (all passing)
**Version:** 0.2.2

---

## Complete

### Core Library (`runtimo-core`)
- Capability trait + registry (name, schema, validate, execute, description)
- Executor pipeline: telemetry → llmosafe gate → execute → WAL
- WAL (append-only JSONL with fsync, rotation, cleanup, tail-read seq recovery)
- Backup manager with cleanup (age-based deletion, integrity verification)
- llmosafe v0.6 integration (ResourceGuard, pressure, entropy)
- Config file (`~/.config/runtimo/config.toml`) with env var override
- Session tracking with persistence and resume
- Health monitor (background snapshots every 60s, CPU/RAM alerts)
- Process tracking with lineage (PPID)
- Path validation (traversal, null byte, non-ASCII, symlink, prefix enforcement)
- Discovery-based telemetry (NVIDIA/AMD/TPU/DRM accelerators, services via pgrep)
- Undo restore target path validation

### Capabilities
- **FileRead** — traversal protection, O_NOFOLLOW, binary detection, UTF-8 safe truncation, JSON auto-parse, max_bytes support
- **FileWrite** — backup-before-mutate, undo support, append mode, dry-run, atomic write, critical file denylist, disk space pre-check
- **ShellExec** — `sh -c` execution, timeout enforcement, descendant kill, dangerous command blocklist, WAL audit
- **Kill** — POSIX signal support, protected PID list, PID reuse prevention
- **GitExec** — clone/pull/commit/revert/clean/status, URL sanitization, secret file detection, SSRF protection
- **Undo** — restore from backup via job ID, path validation on restore targets

### CLI (`runtimo`)
- `run` — execute capability with telemetry + WAL, `--timeout`, `--dry-run`, `--json`, `--quiet`, `--schema`
- `list` — available capabilities with descriptions and schemas
- `telemetry` — hardware report with discovery-based detection
- `processes` — process snapshot
- `status` — job history from WAL
- `logs` — WAL event viewer (filterable by job ID, limit)
- `undo` — restore from backup with path validation
- `config` — manage allowed paths (add/remove/list)

---

## Remaining

### P1: CLI
- [ ] `-f/--args-file` flag to pass args as file (fixes JSON escaping issues)

### P2: Capabilities
- [ ] HTTP request capability (via reqwest)
- [ ] Concurrent job execution with worker pool
- [ ] Backup cleanup policy (TTL-based deletion)

### P3: Daemon
- [ ] Full JSON-RPC server implementation
- [ ] Process isolation (subprocess with cgroups/namespaces)
- [ ] True pre-emptive timeout enforcement

### P4: Monitoring
- [ ] Time-series database for resource usage
- [ ] Prometheus metrics export

### P5: Documentation
- [ ] "How to add a new capability" runbook
- [ ] "How to recover from runaway jobs" runbook
