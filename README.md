# Runtimo

**Capability runtime with telemetry, WAL, and process tracking.**

[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## What Is Runtimo?

Runtimo is a Rust workspace providing a **capability execution engine**. Every capability execution is wrapped with:

- **Telemetry** — Hardware (CPU, RAM, disk, accelerators, services, network) + process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — `llmosafe` circuit breaker reads `/proc/stat` and `/proc/self/status`; rejects execution when pressure exceeds 80%
- **Write-ahead log** — Append-only, fsync'd JSONL event log with crash recovery
- **Backup/undo** — Files backed up before mutation, rollback by job ID
- **Input validation** — Capabilities validate arguments including path traversal, symlink, and null byte protection

**Version:** 0.8.0 | **Rust Edition:** 2021 | **Tests:** 401

## Quick Start

### Library

```bash
cargo add runtimo-core
```

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileRead;
let args = json!({"path": "/tmp/test.txt"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;

println!("Success: {}  Job: {}  WAL seq: {}", result.success, result.job_id, result.wal_seq);
```

### CLI

```bash
cargo build --release

# List capabilities
runtimo list

# Read a file
runtimo run -c FileRead -a '{"path":"/tmp/hostname.txt"}'

# Write a file (creates automatic backup)
runtimo run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'

# Shell command
runtimo run -c ShellExec -a '{"cmd":"ls | head -3"}'

# Dry run (validate without executing)
runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run

# View system telemetry
runtimo telemetry

# View process snapshot
runtimo processes

# View WAL events
runtimo logs

# Undo a job
runtimo undo -j <job_id>

# Show current configuration
runtimo config show

# Set DAL level (A=strict, E=permissive)
runtimo config dal B
```

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│ runtimo CLI                                                    │
│ (run, list, telemetry, processes, logs, status, undo, config)  │
└──────────────────────────┬─────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────┐
│ CapabilityRegistry                                                       │
│ ┌──────────┐ ┌──────────┐ ┌────────┐ ┌──────────┐ ┌──────┐ ┌──────┐ ┌─────────┐│
│ │ FileRead │ │FileWrite │ │ Delete │ │ShellExec │ │ Undo │ │ Kill │ │ GitExec ││
│ └──────────┘ └──────────┘ └────────┘ └──────────┘ └──────┘ └──────┘ └─────────┘│
│ └──────────┘ └──────────┘ └──────────┘ └──────┘ └──────┘ └─────────┘│
└──────────────────────────┬─────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────┐
│ execute_with_telemetry()                                       │
│                                                                │
│ 1. Telemetry::capture()         — hardware + service discovery │
│ 2. ProcessSnapshot::capture()   — process list with PPIDs      │
│ 3. LlmoSafeGuard::check()       — resource guard (80% ceiling) │
│ 4. WalWriter::append(Started)   — WAL event (fsync)            │
│ 5. capability.validate()        — schema + path checks         │
│ 6. capability.execute()         — run the capability           │
│ 7. Telemetry::capture()         — after snapshot               │
│ 8. ProcessSnapshot::capture()   — after snapshot               │
│ 9. WalWriter::append(Completed) — WAL event (fsync)            │
│                                                                │
│ Returns: ExecutionResult with before/after telemetry           │
└────────────────────────────────────────────────────────────────┘
```

## Available Capabilities

### FileRead

Read file contents. Validates path exists, is a file, no traversal.

| Field | Type | Required? |
|-------|------|-----------|
| `path` | string | yes |
| `max_bytes` | integer (1–10,485,760) | no |

**Limit:** 10 MB max file size. Binary files detected and base64-encoded. JSON files auto-parsed. UTF-8 boundary safe.

```bash
runtimo run -c FileRead -a '{"path":"/tmp/data.txt"}'
```

### FileWrite

Write file content with backup-before-mutate for undo support. Appends supported.

| Field | Type | Required? |
|-------|------|-----------|
| `path` | string | yes |
| `content` | string | yes |
| `append` | boolean | no |

**Limit:** 100 MB max content. 100 MB max cumulative file size for append. 10 MB minimum free disk required. Critical files blocked (`.env`, `.env.*`, `.bashrc`, `.ssh/authorized_keys`, etc.).

```bash
runtimo run -c FileWrite -a '{"path":"/tmp/out.txt","content":"hello"}'
runtimo run -c FileWrite -a '{"path":"/tmp/log.txt","content":"\nline 2","append":true}'
```

### Delete

Delete a file with backup-before-delete. Path is validated against the
allowed-prefix whitelist, critical files (`.bashrc`, `.ssh/*`, `.env`, …) are
blocked, and a backup is created so `Undo` can restore the file by job. This is
the audited alternative to `rm`, which remains hard-blocked in ShellExec (use it
for stale lockfiles like `/tmp/libtpu_lockfile`).

| Field | Type | Required? |
|-------|------|-----------|
| `path` | string | yes |
| `no_backup` | boolean (default `false`) | no |

```bash
runtimo run -c Delete -a '{"path":"/tmp/libtpu_lockfile"}'
# find the job ID with `runtimo jobs`, then restore with:
runtimo run -c Undo -a '{"job_id":"<id>"}'

# Skip backup for huge files under disk pressure (irreversible):
runtimo run -c Delete -a '{"path":"/models/llama-70b.safetensors","no_backup":true}'
```

### ShellExec

Execute shell commands via `sh -c`. Supports pipes, redirects, chaining, variables. Enforces timeout and multi-layer dangerous command blocklist with quoting-bypass detection.

| Field | Type | Required? |
|-------|------|-----------|
| `cmd` | string | yes |
| `timeout_secs` | integer (≥1, no upper bound) | no |

**Multi-layer security:**
| Layer | What it blocks |
|-------|----------------|
| **Detokenized blocklist** | `rm`, `shred`, `mkfs`, `fdisk`, `dd`, `shutdown`, `reboot`, `halt`, `poweroff`, `chown`, `chgrp`, `mount`, `umount`, `iptables`, `nft`, `chmod`, `killall`, fork bombs (`:(){`), env dumpers |
| **Quoting bypass** | Normalizes `r"m"`, `$'rm'`, backslash escapes before blocklist check |
| **Regex patterns** | Catches `rm -rf /`, `rm --recursive /`, `rm -r --no-preserve-root` regardless of flag order |
| **PATH sanitization** | Forced `PATH=/usr/local/bin:/usr/bin:/bin` before spawn |
| **Network gating** | `curl`, `wget`, `nc`, `ssh`, etc. blocked — opt-in via `RUNTIMO_ENABLE_NETWORK=1` (or config `[env]`) |
| **Process isolation** | Process group, `SIGKILL` fallback on timeout (default 30s, no upper bound), PID tracking |
| **Output caps** | Stdout/stderr capped at 10 MB |

> **Note:** The ShellExec timeout (default 30s) is the hard kill — it has no
> upper bound, so long-running jobs (training, inference, XLA/vLLM first
> compile) can set any value ≥ 1. The executor-level timeout (`--timeout`,
> config `[capability_timeouts] ShellExec`) is also honored.
>
> **Defense opt-outs** (config.toml, all default to enabled): `blocklist_enabled
> = false` turns ShellExec into plain `sh -c` with no dangerous-command
> filtering; `path_sanitization_enabled = false` inherits the caller's `PATH`.
> `path_restriction_enabled = false` removes the allowed-prefix whitelist and
> ShellExec path scan; `critical_files_enabled = false` allows writing/deleting
> critical files. Safe defaults apply when unset. See `runtimo config show`.

```bash
runtimo run -c ShellExec -a '{"cmd":"uptime"}'
runtimo run -c ShellExec -a '{"cmd":"ls | head -5"}'
runtimo run -c ShellExec -a '{"cmd":"echo hi && whoami"}'

# These are all blocked — no execution happens:
runtimo run -c ShellExec -a '{"cmd":"rm -rf /"}'
runtimo run -c ShellExec -a "{\"cmd\":\"r\\\"m\\\" -rf /\"}"  # quoting bypass
# → blocked: dangerous command blocked: rm command blocked
```

### Undo

Restore files from backup using job ID. Reads WAL to find original paths, validates restore targets against allowed prefixes.

| Field | Type | Required? |
|-------|------|-----------|
| `job_id` | string | yes |

```bash
runtimo undo -j abc123
```

### Kill

Terminate a process by PID with signal support. Protected PIDs (init, kthreadd, self, parent) cannot be killed. Includes PID reuse protection via `/proc/{pid}/stat` start-time comparison.

| Field | Type | Required? |
|-------|------|-----------|
| `pid` | integer (≥1) | yes |
| `signal` | integer (-64–64) | no |

```bash
runtimo run -c Kill -a '{"pid":12345}'
runtimo run -c Kill -a '{"pid":12345,"signal":9}'
```

### GitExec

Git operations (clone, pull, commit, revert, clean, status). URL sanitization, SSRF blocking, secret file detection, branch/commit validation.

| Field | Type | Required? |
|-------|------|-----------|
| `operation` | string (clone\|pull\|commit\|revert\|clean\|status) | yes |
| `url` | string | no |
| `path` | string | no |
| `branch` | string | no |
| `message` | string | no |
| `files` | array of strings | no |
| `commit_sha` | string | no |
| `timeout_secs` | integer (1–600) | no |

```bash
runtimo run -c GitExec -a '{"operation":"status","path":"/tmp/repo"}'
runtimo run -c GitExec -a '{"operation":"clone","url":"https://github.com/user/repo.git","path":"/tmp/repo"}'
```

## Safety Model

| Layer | Mechanism | What it does |
|-------|-----------|--------------|
| **Path validation** | `validate_path()` | Rejects traversal (`..`), null bytes, non-ASCII, symlink escapes. Enforces allowed prefix whitelist (`/tmp`, `/var/tmp` + config). |
| **Critical file deny** | `is_critical_file()` | Blocks `.bashrc`, `.ssh/authorized_keys`, `.gitconfig`, `.netrc`, etc. |
| **Resource guard** | `LlmoSafeGuard` | Reads `/proc/stat` + `/proc/self/status`. Rejects execution when pressure > 80%. Rolling average over 30s. Cooldown persists across restarts. |
| **Zombie guard** | Executor pre-check | Rejects execution if zombie count > 10. |
| **Args size guard** | Executor pre-check | Rejects capability arguments > 1 MB. |
| **Disk space check** | `check_disk_space()` | Runs `df -B1`, parses header-aware "Available" column. Requires 10 MB free. |
| **WAL audit** | `WalWriter` | Every job start/completion/failure written to append-only JSONL with fsync. Sequence recovery, rotation, cleanup. |
| **Backup/undo** | `BackupManager` | Backup before mutate. Integrity verified (size comparison). Restore validates target paths against allowed prefixes. |
| **Shell timeout** | `wait_with_timeout()` | ShellExec kills entire process group on timeout. |
| **Kill protection** | Protected PID list + PID reuse check | init(1), kthreadd(2), self, and parent PIDs cannot be killed. Start-time comparison prevents wrong-target kills. |

## Telemetry

### Discovery-Based Detection

Telemetry detects what's running — no assumptions about hardware or services.

| Category | What it detects | How |
|----------|-----------------|-----|
| **CPU** | Model, count | `/proc/cpuinfo` |
| **RAM** | Total, free, available | `/proc/meminfo` |
| **Disk** | Total, used, available % | `df -h` |
| **Accelerators** | NVIDIA, AMD, TPU, DRM | `nvidia-smi`, `rocm-smi`, `/dev/accel*`, `/dev/dri/render*` |
| **Services** | vLLM, nginx, postgres, redis, docker | Use `pgrep` directly |
| **Network** | Public IP, interfaces, tunnel status | `curl`, `/sys/class/net/*` |
| **Processes** | Full list, zombies, top consumers, PPID chain | `ps aux` + `/proc` |

Unavailable hardware/services are simply absent from output — no "not installed" noise.

```bash
runtimo telemetry       # human-readable report
runtimo telemetry --json # machine-readable
```

## WAL Events

All events written to append-only JSONL with fsync:

| Event Type | When |
|------------|------|
| `job_started` | Before validation |
| `job_completed` | After successful execution |
| `job_failed` | On validation or execution failure |
| `command_executed` | (Debug builds only) Shell command with stdout/stderr/exit code |

```bash
runtimo logs                    # last 10 events
runtimo logs -n 50              # last 50 events
runtimo logs -j <job_id>        # filter by job
runtimo status                  # job summaries from WAL
```

## Observe — L1 sampling without modifying the target

Observe is a sibling collector that samples a target process out-of-process (P1A) and writes a WAL-backed, hash-chained bundle; it never injects code into the target, never uses `ptrace` stop >1 ms, and never uses `LD_PRELOAD`. Every tick is gated through `LlmoSafeGuard::execute` with `ObserveBudget::should_suspend` (pressure >80% suspends). Bounded channels (512) drop newest on overflow and emit a `TRUNCATED` marker — never silent zeros; fallback on `EPERM`/unknown runtime emits a `TRUNCATED`/`SAMPLED` marker with an error note. Sampling is L1 only: stack snapshots at 50 Hz plus exhaustive low-volume audit for imports/spawns/raises/dynamic loads — it does NOT provide L2 line/branch coverage; do not use for line-level CUT decisions. Source: `core/src/observe/mod.rs:1-18`, `core/src/observe/sampler.rs:1-21`, `core/src/observe/supervisor.rs:1-14`, `cli/src/main.rs:274-285`, `cli/src/main.rs:288-295`.

### DAL-graded rigor

Collector failures never kill the target; only the bundle watermark changes via the DAL ladder (`core/src/observe/supervisor.rs:76-107`, `core/src/observe/supervisor.rs:268-301`, `core/src/llmosafe.rs:210` mirror).

| DAL | Collector decision | Bundle watermark | Meaning |
|-----|------------------|----------------|---------|
| A | `Halt` | `INCOMPLETE` | Strict: shed collection, bundle incomplete (`core/src/observe/supervisor.rs:102-103`). Target still exits naturally — collector `Halt` never kills the target. |
| B | `Degraded` | `Truncated` | `Halt→Escalate` maps to `Truncated` with markers (`core/src/observe/supervisor.rs:104`). |
| C | `Degraded` | `Truncated` | `Halt/Escalate→Warn` maps to `Truncated` (`core/src/observe/supervisor.rs:105`). |
| D | `Degraded` | `Truncated` | Same as C (`core/src/observe/supervisor.rs:105`). |
| E | `Proceed` | `Truncated` | Permissive: keep sampling with markers (`core/src/observe/supervisor.rs:106`). |

Five honest failure reasons: `collector-killed`, `disk-full`, `clock-skew`, `restart`, `pressure-spike` (`core/src/observe/supervisor.rs:45-73`). Every `ObserveSuspended`/`ObserveTruncated` event carries `"note": "target never signalled; collector Halt never kills target"` (`core/src/observe/supervisor.rs:296`).

### CLI reference

```bash
runtimo observe --pid <PID>                  # attach to live pid
runtimo observe --cmd "python app.py"        # spawn as sibling target (shares parent with collector)
runtimo observe --pid 123 --out /tmp/b.jsonl # explicit bundle path (validated via allowed prefixes + data_dir)
runtimo observe --sample-rate-hz 50          # 0→50, >1000→1000 cap (core/src/observe/sampler.rs:196-206)
runtimo observe --dal A                      # A–E, case-insensitive, default from config (core/src/observe/supervisor.rs:174-180)
runtimo observe --self-test                  # 4 checks, exit 0/1
runtimo observe --verify /path/bundle.jsonl  # offline verify, prints trailer
runtimo observe --pid 123 --json             # JSON output
```

Flags derived from code: `cli/src/main.rs:296-324` `Commands::Observe { pid, cmd, out, sample_rate_hz, burst, dal, self_test, verify, json }`. `--burst` exists but is deferred (P2B bundle-path watch not yet implemented — prints `note: --burst ... deferred` and uses polling, `cli/src/main.rs:634-636`; daemon surfaces `-32601 observe_burst deferred`, `daemon/src/engine.rs:110-117`). Global flags `--output`, `--color/--no-color`, `--emoji/--no-emoji`, `--table-style`, `--timestamps/--no-timestamps` apply as for other subcommands.

Exit codes:

| Invocation | 0 | 1 |
|------------|---|---|
| `--self-test` | all 4 checks `ok` (`core/src/observe/self_test.rs:52-76`) | at least one `FAIL` |
| `--verify <path>` | `hash_ok == true && error == None` (`cli/src/main.rs:630-632`) | hash mismatch or read error; also prints `truncated_gaps` |
| `--pid`/`--cmd` normal | bundle finalized + `verify.total`/`hash_ok` printed (`cli/src/main.rs:2719-2733`) | invalid path, spawn failure, or finalize failure |

#### --help (verbatim, `runtimo observe --help`, 2026-09-04, exit 0)

```
Observe — out-of-process sampling (P1A) with sibling supervision.
No in-target code, no LD_PRELOAD, no stop >1 ms.
Bundles are WAL-backed with hash chains and TRUNCATED markers.

CUT WARNING: L1 sampling only — not L2 line/branch coverage.
Use --self-test to verify the pipeline and --verify to check bundle integrity.
Default out: {data_dir}/bundles/<run_id>.jsonl (7d retention).
Rate from --sample-rate-hz or RUNTIMO_OBSERVE_SAMPLE_HZ or config observe.sample_rate_hz (default 50 Hz).

Usage: runtimo observe [OPTIONS]

Options:
      --output <FORMAT>
          Output format: human|json|plain|quiet
      --pid <PID>
          Target pid to sample (alternative to --cmd)
      --cmd <CMD>
          Command to spawn as sibling target (alternative to --pid, e.g. "python app.py")
      --color
          Enable ANSI color (requires tty, honored only when explicitly set; --no-color wins, NO_COLOR env forces off)
      --no-color
          Disable ANSI color (wins over --color and NO_COLOR)
      --out <OUT>
          Bundle output path (default: {data_dir}/bundles/<run_id>.jsonl, validated via allowed prefixes)
      --emoji
          Enable emoji (off by default; --no-emoji wins)
      --sample-rate-hz <SAMPLE_RATE_HZ>
          Samples per second (default 50 Hz, via ObserveConfig)
      --burst
          Enable burst file-watch (P2B — bundle-path watch; deferred if non-trivial, currently no-op with note)
      --no-emoji
          Disable emoji
      --dal <DAL>
          DAL A–E (default from config, controls watermark on shed)
      --table-style <STYLE>
          Table style: plain|markdown|box|csv
      --self-test
          Run self-test and exit 0/1
      --timestamps
          Enable timestamps
      --no-timestamps
          Disable timestamps (wins over --timestamps)
      --verify <VERIFY>
          Verify a bundle file offline and print trailer (hash chain + truncated gaps)
      --json
          Output as JSON
  -h, --help
          Print help (see a summary with '-h')
```

Source: live run `cargo build --release && ./target/release/runtimo observe --help` (exit 0). Full output above — global flags (`--output`, `--color/--no-color`, `--emoji/--no-emoji`, `--table-style`, `--timestamps/--no-timestamps`) interleave with observe flags as shown; excerpt untrimmed.

### Bundle file location and verify semantics

Default bundle: `{data_dir}/bundles/<run_id>.jsonl` where `data_dir` is `XDG_DATA_HOME` or `~/.local/share` → `.../runtimo` (`core/src/lib.rs:206-224`), `run_id` is 32 hex chars (`core/src/lib.rs:242-267`), and `bundle_path()` asserts never ending in `wal.jsonl` (`core/src/observe/bundle.rs:43-56`). Explicit `--out` is validated against `RuntimoConfig::get_allowed_prefixes()` plus `data_dir` (`cli/src/main.rs:2603-2654`, `daemon/src/engine.rs:693-712`). Batching: ≤256 events or 100 ms, one `fsync` per batch plus watermark `fsync` on close/drop; checkpoint every 1 000 events or 5 s at `{bundle}.checkpoint` preserving `.jsonl` (`core/src/observe/bundle.rs:27-34`, `core/src/observe/bundle.rs:353-372`). Dual-clock: `mono_ns = base.elapsed()` strictly increasing, `wall_ns` is wall-clock since epoch (`core/src/observe/bundle.rs:81-84`, `core/src/observe/bundle.rs:220-230`).

`verify_bundle(path)` (`core/src/observe/bundle.rs:422-525`) returns:

| Field | Meaning | Failure signal |
|-------|---------|----------------|
| `total` | events read | < expected when corruption makes lines unparseable |
| `truncated_gaps` | seq gaps (with or without `ObserveTruncated` marker) (`core/src/observe/bundle.rs:450-462`) | >0 means data was dropped and accounted |
| `hash_ok` | recomputed `sha256(prev_hash ++ batch_json)` matches `bundle_hash` (`core/src/observe/bundle.rs:463-517`) | `false` on tamper |
| `error` | read/parse error (`core/src/observe/bundle.rs:426-432`) | `Some(...)` |

CLI `--verify` prints `verify <path>: total=… truncated_gaps=… hash_ok=… error=…` and `trailer: bundle … — hash chain ok/FAIL — watermark TRUNCATED/Complete` (`cli/src/main.rs:2616-2628`), exits 0 only if `hash_ok && error.is_none()`. Daemon `observe_verify` does the same read-only check with path validation (`daemon/src/engine.rs:867-911`).

Retention: daemon hourly task runs `WalWriter::cleanup(..., 86400*7)` and `BackupManager::cleanup(..., 86400*7)` — 7 days for WAL and backups/bundles (`daemon/src/engine.rs:1144-1151`, `core/src/config.rs:109` provisional `7d bundle retention — observe provisional 7d`).

### Self-test — 4 checks

`runtimo observe --self-test` (`cli/src/main.rs:2599-2602` → `core/src/observe/self_test.rs:52-76` `run()`) runs `checks()` (`core/src/observe/self_test.rs:44-51`):

| # | Name | What it proves | Source |
|---|------|----------------|--------|
| 1 | `fixture A exactness` | `AuditHook` 10 imports + 2 spawns + 1 raise + 1 dynamic = 14 events exact, `Complete` (no drops, no `TRUNCATED`) | `core/src/observe/self_test.rs:84-115`, `core/src/observe/audit.rs:268-307` |
| 2 | `fixture B sampling bounds` | `OutOfProcessSampler` at 50 Hz, 10 ticks: observed rate within bounds or ≥5 samples, coverage via frames or fallback marker (never silent zeros) | `core/src/observe/self_test.rs:126-176` |
| 3 | `DAL-A gate` | `inject_drop_next` + `PressureSpike` → watermark `INCOMPLETE`, never `COMPLETE` on DAL A; target never signalled | `core/src/observe/self_test.rs:181-221`, `core/src/observe/supervisor.rs:440-463` |
| 4 | `tamper detection` | corrupt one byte → `verify_bundle` reports `!hash_ok || error.is_some() || truncated_gaps>0 || total!=3` | `core/src/observe/self_test.rs:225-292` |

#### Live transcript (verbatim, `runtimo observe --self-test`, exit 0, 2026-09-04)

```
ok  fixture A exactness — 14 events exact (10 imports + 2 spawns + 1 raise + 1 dynamic), Complete
ok  fixture B sampling bounds — hz=50 observed=32.8 got 10/10 coverage=true within bounds
ok  DAL-A gate — DAL A Halt ⇒ Incomplete (never COMPLETE), target never signalled
ok  tamper detection — corruption detected (hash_ok=false total=0 gaps=0 err=Some("read failed: stream did not contain valid UTF-8"))
observe self-test: ok (4 checks)
```

Source: live run `cargo build --release && ./target/release/runtimo observe --self-test` (exit 0).

### Daemon RPC

Unix socket `{data_dir}/runtimo.sock` (`cli/src/main.rs:482`). JSON-RPC line-delimited. Observe methods (`daemon/src/engine.rs:99-117`, `daemon/src/rpc.rs:86-132`):

| Method | Params struct | Effect |
|--------|---------------|--------|
| `observe_start` | `ObserveStartParams { pid, cmd, out, sample_rate_hz, dal, burst, run_id }` (`daemon/src/rpc.rs:87-110`) | Validates `out` (allowed prefixes + `data_dir`), resolves `run_id`/`dal`/`hz` (file/env/default), reserves `BackgroundJob` slot (16 max), `spawn_blocking` `ObserveSupervisor` loop (2 s or 100 ticks, `interval.min(20ms)`, `gated_tick`), never signals target |
| `observe_status` | `ObserveStatusParams { run_id, limit }` (`daemon/src/rpc.rs:113-125`) | Queries `BackgroundJobRegistry` then WAL `ObserveStarted`/`ObserveCompleted` fallback; list mode filters `capability=="Observe"` (`daemon/src/engine.rs:806-865`) |
| `observe_verify` | `ObserveVerifyParams { path }` (`daemon/src/rpc.rs:128-132`) | Validates path, calls `bundle::verify_bundle` read-only, returns `{ path, total, truncated_gaps, hash_ok, error }` (`daemon/src/engine.rs:867-911`) |
| `observe_burst` | — | **DEFERRED — not GA.** Returns `-32601 observe_burst deferred: P2B file-watch burst not yet implemented (use out-of-process polling; see audit.rs)` (`daemon/src/engine.rs:110-117`). Do not document as working. |

Burst note: CLI `--burst` prints `note: --burst file-watch burst (P2B bundle-path watch) deferred — not yet trivial; using polling` (`cli/src/main.rs:634-636`); daemon `observe_start` logs `observe burst (P2B bundle-path watch) deferred — using polling` when `burst==true` (`daemon/src/engine.rs:688-690`).

### Configuration

`core/src/config.rs:95-122` `ObserveConfig`, `core/src/config.rs:153-158` `ResolvedConfig` fields.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `observe.sample_rate_hz` | `u64` | `50` | Samples per second; `0` coerces to 50, >1000 caps to 1000 (`core/src/observe/sampler.rs:196-206`, `core/src/observe/supervisor.rs:171`). |
| `observe.pressure_suspend_ms` | `u64` | `1000` | Suspension window under high pressure (`core/src/config.rs:637`). |
| `observe.max_bundle_bytes` | `u64` | `10485760` (10 MiB) | Bundle size before rotation (`core/src/config.rs:641`). `unverified` — rotation wiring not yet observed; how to verify: grep daemon `engine.rs` for `observe_max_bundle_bytes` usage. |

Precedence (highest to lowest): CLI `--sample-rate-hz` > env `RUNTIMO_OBSERVE_SAMPLE_HZ` > file `observe.sample_rate_hz` > default `50` (`core/src/config.rs:97-98`, `core/src/config.rs:622-693` `resolved()` and `effective_observe_sample_hz()`). Malformed env value falls through to file, not to hard 50 (`core/src/config.rs:625-629`, `core/src/config.rs:681-688`). Example:

```toml
# $XDG_CONFIG_HOME/runtimo/config.toml
[observe]
sample_rate_hz = 50
pressure_suspend_ms = 1000
max_bundle_bytes = 10485760
```

```bash
RUNTIMO_OBSERVE_SAMPLE_HZ=100 runtimo observe --pid 123      # env beats file
runtimo observe --pid 123 --sample-rate-hz 25                # CLI beats env
```

Verify: `RUNTIMO_OBSERVE_SAMPLE_HZ=bad runtimo observe --pid $$ --json` falls through to file (`core/src/config.rs:681-688` — `unverified` live, how to verify: run with bad env and inspect `hz` in JSON output).

### Safety contract

Collector `Halt` never kills the target — only the bundle watermark becomes `INCOMPLETE` (`core/src/observe/supervisor.rs:75-77`, `core/src/observe/supervisor.rs:262-301` emitting `ObserveTruncated`/`ObserveSuspended` with note, never signalling). All bounded channels (sampler 512, audit 512) drop newest and emit a `TRUNCATED` marker; never silent (`core/src/observe/sampler.rs:332-343`, `core/src/observe/audit.rs:189-241`). Secrets are redacted at every WAL boundary: any `target`, `location`, `frame`, or `error` containing `auth_token`/`bearer`/`api_key` (case-insensitive) is replaced with `REDACTED` before serialization (`core/src/observe/audit.rs:61-64`, `core/src/observe/audit.rs:99-137`, `core/src/observe/sampler.rs:71-88`, `core/src/observe/sampler.rs:94-100`). Bundle hash chain detects tamper (`core/src/observe/bundle.rs:463-517`).

## Project Structure

```
runtimo/
├── Cargo.toml              # Workspace definition
├── dist-workspace.toml     # cargo-dist release configuration
├── core/                   # runtimo-core library
│   ├── src/
│   │   ├── lib.rs          # Public exports + error types
│   │   ├── capability.rs   # Capability trait + CapabilityRegistry
│   │   ├── executor.rs     # execute_with_telemetry() pipeline
│   │   ├── job.rs          # Job, JobId, JobState lifecycle
│   │   ├── telemetry.rs    # Hardware + service discovery
│   │   ├── processes.rs    # Process snapshot with PPID tracking
│   │   ├── llmosafe.rs     # llmosafe ResourceGuard integration
│   │   ├── wal.rs          # Write-ahead log (WalWriter, WalReader)
│   │   ├── backup.rs       # BackupManager for undo support
│   │   ├── session.rs      # Session tracking and persistence
│   │   ├── config.rs       # TOML configuration + allowed paths
│   │   ├── observe/        # Observe subsystem (L1 sampling, sibling collector)
│   │   │   ├── mod.rs      # Pipeline overview + invariants
│   │   │   ├── bundle.rs   # WAL-backed, hash-chained bundle writer + verify
│   │   │   ├── budget.rs   # Resource-budget (ResourceHistory sibling)
│   │   │   ├── audit.rs    # Exhaustive low-volume hook (imports/spawns/raises/dynamic loads)
│   │   │   ├── sampler.rs  # Out-of-process /proc stack sampler (512-cap)
│   │   │   ├── supervisor.rs # Sibling supervisor + DAL watermark + honest-mark table
│   │   │   └── self_test.rs # 4-check proof-test (fixtures A/B, DAL-A gate, tamper)
│   │   ├── monitor.rs      # Health monitor (snapshots, alerts)
│   │   ├── cmd.rs          # Shell command execution helper
│   │   ├── validation/     # Unified path validation
│   │   │   ├── mod.rs
│   │   │   └── path.rs     # Path traversal + symlink protection
│   │   └── capabilities/
│   │       ├── mod.rs
│   │       ├── file_read.rs
│   │       ├── file_write.rs
│   │       ├── delete.rs
│   │       ├── shell_exec.rs
│   │       ├── kill.rs
│   │       ├── git_exec.rs
│   │       └── undo.rs
│   ├── tests/
│   │   ├── integration.rs  # 58 integration tests
│   │   └── robust.rs       # 46 property-based tests (6 G-categories)
│   └── examples/
├── cli/                    # runtimo binary (+ runtimo-daemon)
│   └── src/
│       ├── main.rs         # CLI commands via clap
│       └── daemon_bin.rs   # runtimo-daemon binary entrypoint
└── daemon/                 # runtimo-daemon library
    └── src/
        ├── engine.rs       # daemon state + event loop
        ├── rpc.rs          # JSON-RPC message types
        ├── jobs.rs         # background jobs
        ├── auth.rs         # Unix-socket peer auth
        ├── config.rs       # daemon config
        └── dispatch.rs     # capability dispatch
```

## Testing

```bash
cargo test                           # all tests
cargo test -p runtimo-core --lib    # 291 unit tests
cargo test -p runtimo-core --test integration  # 58 integration tests
cargo test -p runtimo-core --test robust       # 46 property-based tests
cargo test -p runtimo-core --doc    # 6 doc tests
cargo clippy --all-targets          # zero warnings required
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNTIMO_WAL_PATH` | `$XDG_DATA_HOME/runtimo/wal.jsonl` | WAL file path |
| `RUNTIMO_SESSIONS_DIR` | `$XDG_DATA_HOME/runtimo/sessions` | Session storage |
| `RUNTIMO_ALLOWED_PATHS` | (colon-separated) | Additional allowed path prefixes |
| `XDG_CONFIG_HOME` | `~/.config` | Config file location (`runtimo/config.toml`) |
| `XDG_DATA_HOME` | `~/.local/share` | Default WAL/backup/session root |
| `RUNTIMO_ENABLE_PUBLIC_IP` | (unset) | Set to `1` to enable public IP discovery in telemetry |
| `RUNTIMO_ENABLE_NETWORK` | (unset) | Set to `1` to allow outbound network tools (curl, wget, ssh, etc.) in ShellExec |
| `RUNTIMO_DAL` | (unset = A) | Design Assurance Level for cognitive safety pipeline (A-E). A=strict, E=permissive. Also configurable via `runtimo config dal` or config file `dal` field. |
| `RUNTIMO_OBSERVE_SAMPLE_HZ` | `50` | Observe samples per second; precedence CLI `--sample-rate-hz` > env > `observe.sample_rate_hz` file > 50 (`core/src/config.rs:623-641`, `core/src/config.rs:680-693`). Malformed env falls through to file. |
| `RUNTIMO_STATE_DIR` | `$XDG_DATA_HOME/runtimo` | Override state directory for WAL/backups/sessions |

## License

MIT — see [LICENSE](LICENSE).
