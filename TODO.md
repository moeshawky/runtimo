# Runtimo Status

**Last Updated:** 2026-09-23
**Build:** `cargo clippy --all-targets` — clean, 0 warnings
**Tests:** ~767 passed / 2 failed same-known (observe_fixture_b_integration, observe_self_test_run_exits_zero) / runtime_fact_export 12/12 (39 cli + 489 core-lib + 69 integration + 46 robust + 15 conformance09 + 12 runtime_fact_export + 8 doctest)
**Version:** 0.10.1 (released) — crate deps pinned ^0.10 (verified: daemon/Cargo.toml:18, cli/Cargo.toml:23-24)

---

## Complete

### Core Library (`runtimo-core`)
- Capability trait + registry (name, schema, validate, execute, description)
- Executor pipeline: telemetry → llmosafe gate → execute → WAL
- WAL (append-only JSONL with fsync, rotation, cleanup, tail-read seq recovery)
- Backup manager with cleanup (age-based deletion, integrity verification)
- llmosafe 0.9.0 integration (ResourceGuard, pressure, entropy; single fresh upstream observation per call, no rolling average, no cooldown cache, `ResourceHistory` deleted)
- Config file (`~/.config/runtimo/config.toml`) with env var override
- Session tracking with persistence and resume
- Health monitor (background snapshots every 60s, CPU/RAM alerts)
- Process tracking with lineage (PPID)
- Path validation (traversal, null byte, non-ASCII, symlink, prefix enforcement)
- Discovery-based telemetry (NVIDIA/AMD/TPU/DRM accelerators, services via pgrep)
- Undo restore target path validation
- Config-based DAL override (`dal` field in config.toml, env `RUNTIMO_DAL`)
- Config-based blocklist overrides (`blocklist_overrides` field)
- Config-based capability timeouts (`capability_timeouts` field)
- Oracle module (`core/src/oracle/`): `PropertySpec`, `Predicate`, `Op`, `Verdict`, `PropertyVerdict`, `evaluate`, `parse_spec` — AND semantics over WAL events, `bundle_hash` never read
- `verify` exit keys off `admissible` (conjunction of 5 predicates + no error)
- `hash_ok` retained as deprecated alias = `structurally_parseable && integrity_valid`
- Per-guard `llmosafe` history: single fresh upstream observation per call; `ResourceHistory` deleted (d95cd1b)
- Burst deferred contract: `observe_burst` RPC → `-32601 observe_burst deferred`; CLI prints `burst_deferred:true` note

### Capabilities
- **FileRead** — traversal protection, O_NOFOLLOW, binary detection, UTF-8 safe truncation, JSON auto-parse, max_bytes support
- **FileWrite** — backup-before-mutate, undo support, append mode, dry-run, atomic write, critical file denylist, disk space pre-check
- **ShellExec** — `sh -c` execution, timeout enforcement, descendant kill, dangerous command blocklist (hardcoded + config overrides), WAL audit, skip_cognitive bypass
- **Kill** — POSIX signal support, protected PID list, PID reuse prevention
- **GitExec** — clone/pull/commit/revert/clean/status, URL sanitization, secret file detection, SSRF protection
- **Undo** — restore from backup via job ID, path validation on restore targets
- **Delete** — backup-before-delete, undo support, path validation, critical file denylist

### CLI (`runtimo`)
- `run` — execute capability with telemetry + WAL, `--timeout` (config-aware), `--dry-run`, `--json`, `--quiet`, `--schema`
- `list` — available capabilities with descriptions and schemas
- `telemetry` — hardware report with discovery-based detection
- `processes` — process snapshot
- `status` — job history from WAL
- `logs` — WAL event viewer (filterable by job ID, limit)
- `undo` — restore from backup with path validation
- `config show` — display current config
- `config dal [A-E]` — set DAL level
- `observe` — L1 sampling, `--verify` (5-predicate), `--self-test` (5 checks), `--burst` deferred (-32601)
- `dispatch` / `wait` — background job dispatch

### Daemon
- JSON-RPC over Unix socket
- Background job dispatch with structured error propagation
- Status response includes error/result field for failed jobs
- `observe_verify` returns full 5-predicate set (`structurally_parseable`, `integrity_valid`, `lifecycle_valid`, `completeness_known`, `admissible`) plus `total`/`truncated_gaps`/`watermark`/`error`, `hash_ok` deprecated alias
- `observe_burst` → `-32601 observe_burst deferred`

---

## Remaining

### P1: CLI
- [x] `-f/--args-file` flag to pass args as file (fixes JSON escaping issues) — done in v0.7.1
- [ ] `--properties` JSON spec evaluation against bundle WAL events (oracle integration) — unverified: how to verify: `runtimo observe --verify <bundle> --properties '<spec>'` and check property verdicts in output

### P2: Capabilities
- [ ] HTTP request capability (via reqwest)
- [ ] Concurrent job execution with worker pool
- [ ] Backup cleanup policy (TTL-based deletion)
- [ ] `max_bundle_bytes` residual — field still in `ObserveConfig`/`ResolvedConfig`/`output.rs`; removal from structs pending; enumerate occurrences; consumer analysis before remove; compile/tests prove no phantom usage

### P3: Daemon
- [ ] Process isolation (subprocess with cgroups/namespaces)
- [ ] True pre-emptive timeout enforcement

### P4: Monitoring
- [ ] Time-series database for resource usage
- [ ] Prometheus metrics export

### P5: Documentation
- [ ] "How to add a new capability" runbook
- [ ] "How to recover from runaway jobs" runbook
