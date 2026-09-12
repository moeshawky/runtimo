# Runtimo

**Capability runtime with telemetry, WAL, and process tracking.**

[![Crates.io](https://img.shields.io/crates/v/runtimo-core.svg)](https://crates.io/crates/runtimo-core)
[![Documentation](https://docs.rs/runtimo-core/badge.svg)](https://docs.rs/runtimo-core)
[![CI](https://github.com/moeshawky/runtimo/actions/workflows/ci.yml/badge.svg)](https://github.com/moeshawky/runtimo/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.70-blue.svg)](rust-toolchain.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

- [x] Project name + one-line description
- [x] Badges row (CI, version, license, MSRV)
- [x] Installation instructions (`cargo install runtimo-cli`)
- [x] Quick start example (compilable)
- [x] API docs link ([docs.rs](https://docs.rs/runtimo-core))
- [x] [CHANGELOG](CHANGELOG.md)
- [x] [License](LICENSE)
- [x] [Contributing](CONTRIBUTING.md)
- [x] MSRV badge (1.70.0)

> **One program, one version.** `runtimo-core`, `runtimo-daemon`, and `runtimo-cli` are a single program at a single workspace version. Never version, bump, or release one crate independently. `cargo install runtimo-cli` installs **both** `runtimo` (CLI) and `runtimo-daemon` binaries. The `runtimo-daemon` package is the daemon *library*; the `runtimo-daemon` *binary* is bundled inside the `runtimo-cli` package. Never `cargo install runtimo-daemon` or `cargo install runtimo-core` alone for deployment.

## What Is Runtimo?

Runtimo is a Rust workspace providing a **capability execution engine**. Every capability execution is wrapped with:

- **Telemetry** — Hardware (CPU, RAM, disk, accelerators, services, network) + process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — `llmosafe` circuit breaker reads `/proc/stat` and `/proc/self/status`; rejects execution when pressure exceeds 80%
- **Write-ahead log** — Append-only, fsync'd JSONL event log with crash recovery
- **Backup/undo** — Files backed up before mutation, rollback by job ID
- **Input validation** — Capabilities validate arguments including path traversal, symlink, and null byte protection

**Version:** 0.9.1 | **Rust Edition:** 2021 | **Tests:** 620 (39 cli + 400 core-lib + 65 integration + 46 robust + 63 daemon + 7 doctest)

See [CHANGELOG.md](CHANGELOG.md) for full release history.

## Quick Start

### Install (one command, both binaries)

```bash
cargo install runtimo-cli
```

This installs **both** `runtimo` (CLI) and `runtimo-daemon` binaries. The daemon auto-starts on `runtimo run`/`runtimo dispatch` via `{data_dir}/runtimo.sock`.

```bash
# Verify both binaries are present
runtimo --help
runtimo-daemon --help

# Show current configuration
runtimo config show
```

### Library (for crate consumers)

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

## Integrate in 5 Minutes

Copy-paste every line. No inference needed.

```bash
# 1. Install (both runtimo + runtimo-daemon binaries)
cargo install runtimo-cli

# 2. Verify config and allowed paths
runtimo config show
# Allowed paths: /tmp + /var/tmp (extend via RUNTIMO_ALLOWED_PATHS or config.toml)

# 3. Run FileRead on /tmp (allowed path)
runtimo run -c FileRead -a '{"path":"/tmp/hostname.txt"}' --args-file /tmp/args.json

# 4. Dispatch a background job and wait
runtimo dispatch -c FileRead -a '{"path":"/tmp/hostname.txt"}'
runtimo wait --job <job_id>

# 5. Check jobs and WAL logs
runtimo jobs
runtimo logs

# 6. Verify output integrity (exit 0 iff admissible)
runtimo observe --verify /tmp/bundle.jsonl
# Must assert admissible, NOT hash_ok (hash_ok is deprecated)

# 7. Undo a job by ID
runtimo undo -j <job_id>

# 8. View telemetry
runtimo telemetry
runtimo processes
```

**Critical notes — no agent may infer these:**
- **Allowed paths**: Only `/tmp` and `/var/tmp` are built-in. All file paths must resolve under these prefixes (or `RUNTIMO_ALLOWED_PATHS`). Use `--args-file` for payloads >130 KB.
- **`--verify` exits on `admissible`**, not `hash_ok`. `hash_ok` is a deprecated alias (`structurally_parseable && integrity_valid`). Scripts must assert `admissible`.
- **`runtimo-daemon` is a library**, not a standalone install target. The binary is bundled with `runtimo-cli`. Never `cargo install runtimo-daemon`.
- **`observe --burst` is deferred** (returns `-32601 observe_burst deferred`), not broken.

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

Observe is a sibling collector that samples a target process out-of-process (P1A) and writes a WAL-backed, hash-chained bundle; it never injects code into the target, never uses `ptrace` stop >1 ms, and never uses `LD_PRELOAD`. Every tick is gated through `LlmoSafeGuard::execute` with `ObserveBudget::should_suspend` (pressure >80% suspends). Bounded channels (512) drop newest on overflow and emit a `TRUNCATED` marker — never silent zeros; fallback on `EPERM`/unknown runtime emits a `TRUNCATED`/`SAMPLED` marker with an error note. Sampling is L1 only: stack snapshots at 50 Hz plus exhaustive low-volume audit for imports/spawns/raises/dynamic loads — it does NOT provide L2 line/branch coverage; do not use for line-level CUT decisions. Source: `core/src/observe/mod.rs`, `core/src/observe/sampler.rs`, `core/src/observe/supervisor.rs`, `cli/src/main.rs` (observe subcommand), `daemon/src/engine.rs` (observe handlers).

### DAL-graded rigor

Collector failures never kill the target; only the bundle watermark changes via the DAL ladder (`core/src/observe/supervisor.rs`, `core/src/llmosafe.rs`).

| DAL | Collector decision | Bundle watermark | Meaning |
|-----|------------------|----------------|---------|
| A | `Halt` | `INCOMPLETE` | Strict: shed collection, bundle incomplete (`core/src/observe/supervisor.rs`). Target still exits naturally — collector `Halt` never kills the target. |
| B | `Degraded` | `Truncated` | `Halt→Escalate` maps to `Truncated` with markers (`core/src/observe/supervisor.rs`). |
| C | `Degraded` | `Truncated` | `Halt/Escalate→Warn` maps to `Truncated` (`core/src/observe/supervisor.rs`). |
| D | `Degraded` | `Truncated` | Same as C (`core/src/observe/supervisor.rs`). |
| E | `Proceed` | `Truncated` | Permissive: keep sampling with markers (`core/src/observe/supervisor.rs`). |

Five honest failure reasons: `collector-killed`, `disk-full`, `clock-skew`, `restart`, `pressure-spike` (`core/src/observe/supervisor.rs`). Every `ObserveSuspended`/`ObserveTruncated` event carries `"note": "target never signalled; collector Halt never kills target"` (`core/src/observe/supervisor.rs`).

### CLI reference

```bash
runtimo observe --pid <PID>                  # attach to live pid
runtimo observe --cmd "python app.py"        # spawn as sibling target (shares parent with collector)
runtimo observe --pid 123 --out /tmp/b.jsonl # explicit bundle path (validated via allowed prefixes + data_dir)
runtimo observe --sample-rate-hz 50          # 0→50, >1000→1000 cap (core/src/observe/sampler.rs)
runtimo observe --dal A                      # A–E, case-insensitive, default from config (core/src/observe/supervisor.rs)
runtimo observe --self-test                  # 4 checks, exit 0/1
runtimo observe --verify /path/bundle.jsonl  # offline verify, prints trailer
runtimo observe --pid 123 --json             # JSON output
runtimo observe --properties '<spec>'        # property spec JSON, verdicts reported but never alter verify exit code
runtimo observe --suspend-ms <MS>            # override pressure_suspend_ms from config
# Global flags: --output, --color/--no-color, --emoji/--no-emoji, --table-style, --timestamps/--no-timestamps
```

Flags derived from code: `cli/src/main.rs` (Observe subcommand), `daemon/src/engine.rs` (burst handler). `--burst` exists but is deferred (P2B bundle-path watch not yet implemented — prints `note: --burst ... deferred` and uses polling, `daemon/src/engine.rs`). Global flags (`--output`, `--color/--no-color`, `--emoji/--no-emoji`, `--table-style`, `--timestamps/--no-timestamps`) apply as for other subcommands.

Exit codes:

| Invocation | 0 | 1 |
|------------|---|---|
| `--self-test` | all 4 checks `ok` (`core/src/observe/self_test.rs`) | at least one `FAIL` |
| `--verify <path>` | `admissible` (`cli/src/main.rs`) | hash mismatch or read error; also prints `truncated_gaps` |
| `--pid`/`--cmd` normal | bundle finalized + `verify.total`/`hash_ok` printed (`cli/src/main.rs`) | invalid path, spawn failure, or finalize failure |

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

Default bundle: `{data_dir}/bundles/<run_id>.jsonl` where `data_dir` is `XDG_DATA_HOME` or `~/.local/share` → `.../runtimo` (`core/src/lib.rs`), `run_id` is 32 hex chars (`core/src/lib.rs`), and `bundle_path()` asserts never ending in `wal.jsonl` (`core/src/observe/bundle.rs`). Explicit `--out` is validated against `RuntimoConfig::get_allowed_prefixes()` plus `data_dir` (`cli/src/main.rs`, `daemon/src/engine.rs`). Batching: ≤256 events or 100 ms, one `fsync` per batch plus watermark `fsync` on close/drop; checkpoint every 1 000 events or 5 s at `{bundle}.checkpoint` preserving `.jsonl` (`core/src/observe/bundle.rs`). Dual-clock: `mono_ns = base.elapsed()` strictly increasing, `wall_ns` is wall-clock since epoch (`core/src/observe/bundle.rs`).

`verify_report(path)` returns five predicates plus metadata. Exit 0 from `--verify` iff `admissible` (conjunction of all predicates and no error):

| Field | Meaning | Failure signal |
|-------|---------|----------------|
| `structurally_parseable` | all lines parse as valid `WalEvent` with valid UTF-8 and non-empty content | `false` on malformed/empty lines |
| `integrity_valid` | hash chain verifies; no tamper, prev-break, or hash-absent events | `false` on tamper |
| `lifecycle_valid` | no missing-first-terminal, duplicate seqs, reopen, or run-id-mismatch | `false` on lifecycle violations |
| `completeness_known` | all seq gaps carry `ObserveTruncated` markers | `false` on unaccounted gaps |
| `admissible` | conjunction of the four predicates plus `error` is `None` | `false` on any failure |
| `hash_ok` | **deprecated alias** for `structurally_parseable && integrity_valid` | retained for backward compatibility |
| `total` | events read | < expected when corruption makes lines unparseable |
| `truncated_gaps` | seq gaps (with or without `ObserveTruncated` marker) | >0 means data was dropped and accounted |
| `error` | read/parse error | `Some(...)` |
| `watermark` | honest watermark from final `ObserveCompleted` event | `None` when no marker present |

CLI `--verify` prints `verify <path>: structurally_parseable=… integrity_valid=… lifecycle_valid=… completeness_known=… admissible=… error=… watermark=…` and `trailer: bundle … — hash chain ok/FAIL — watermark TRUNCATED/Complete`, exits 0 only if `admissible`. Daemon `observe_verify` does the same read-only check with path validation, returning the full predicate set with `hash_ok` retained as a deprecated alias.

Retention: daemon hourly task runs `WalWriter::cleanup(..., 86400*7)` and `BackupManager::cleanup(..., 86400*7)` — 7 days for WAL and backups/bundles (`daemon/src/engine.rs`, `core/src/config.rs` provisional `7d bundle retention — observe provisional 7d`).

### Self-test — 4 checks

`runtimo observe --self-test` (`cli/src/main.rs` → `core/src/observe/self_test.rs` `run()`) runs `checks()` (`core/src/observe/self_test.rs`):

| # | Name | What it proves | Source |
|---|------|----------------|--------|
| 1 | `fixture A exactness` | `AuditHook` 10 imports + 2 spawns + 1 raise + 1 dynamic = 14 events exact, `Complete` (no drops, no `TRUNCATED`) | `core/src/observe/self_test.rs`, `core/src/observe/audit.rs` |
| 2 | `fixture B sampling bounds` | `OutOfProcessSampler` at 50 Hz, 10 ticks: observed rate within bounds or ≥5 samples, coverage via frames or fallback marker (never silent zeros) | `core/src/observe/self_test.rs` |
| 3 | `DAL-A gate` | `inject_drop_next` + `PressureSpike` → watermark `INCOMPLETE`, never `COMPLETE` on DAL A; target never signalled | `core/src/observe/self_test.rs`, `core/src/observe/supervisor.rs` |
| 4 | `tamper detection` | corrupt one byte → `verify_bundle` reports `!v.hash_ok || v.error.is_some() || v.truncated_gaps > 0 || v.total != 3` | `core/src/observe/self_test.rs` |

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

Unix socket `{data_dir}/runtimo.sock` (`cli/src/main.rs`). JSON-RPC line-delimited. Observe methods (`daemon/src/engine.rs`, `daemon/src/rpc.rs`):

| Method | Params struct | Effect |
|--------|---------------|--------|
| `observe_start` | `ObserveStartParams { pid, cmd, out, sample_rate_hz, dal, burst, run_id }` (`daemon/src/rpc.rs`) | Validates `out` (allowed prefixes + `data_dir`), resolves `run_id`/`dal`/`hz` (file/env/default), reserves `BackgroundJob` slot (16 max), `spawn_blocking` `ObserveSupervisor` loop (2 s or 100 ticks, `interval.min(20ms)`, `gated_tick`), never signals target |
| `observe_status` | `ObserveStatusParams { run_id, limit }` (`daemon/src/rpc.rs`) | Queries `BackgroundJobRegistry` then WAL `ObserveStarted`/`ObserveCompleted` fallback; list mode filters `capability=="Observe"` (`daemon/src/engine.rs`) |
| `observe_verify` | `ObserveVerifyParams { path }` (`daemon/src/rpc.rs`) | Validates path, calls `bundle::verify_report` read-only, returns full predicate set with `hash_ok` deprecated alias (`daemon/src/engine.rs`) |
| `observe_burst` | — | **DEFERRED — not GA.** Returns `-32601 observe_burst deferred: P2B file-watch burst not yet implemented (use out-of-process polling; see audit.rs)` (`daemon/src/engine.rs`). Do not document as working. |

Burst note: CLI `--burst` prints `note: --burst file-watch burst (P2B bundle-path watch) deferred — not yet trivial; using polling` (`cli/src/main.rs`); daemon `observe_start` logs `observe burst (P2B bundle-path watch) deferred — using polling` when `burst==true` (`daemon/src/engine.rs`).

### Configuration

`core/src/config.rs` `ObserveConfig`, `core/src/config.rs` `ResolvedConfig` fields.

| Key | Type | Default | Meaning |
|-----|------|---------|---------|
| `observe.sample_rate_hz` | `u64` | `50` | Samples per second; `0` coerces to 50, >1000 caps to 1000 (`core/src/observe/sampler.rs`, `core/src/observe/supervisor.rs`). |
| `observe.pressure_suspend_ms` | `u64` | `1000` | Suspension window under high pressure (`core/src/config.rs`). |
| `observe.max_bundle_bytes` | `u64` | `10485760` (10 MiB) | Bundle size before rotation (`core/src/config.rs`). **Unverified** — rotation wiring not yet observed; how to verify: grep daemon `engine.rs` for `observe_max_bundle_bytes` usage. |

Precedence (highest to lowest): CLI `--sample-rate-hz` > env `RUNTIMO_OBSERVE_SAMPLE_HZ` > file `observe.sample_rate_hz` > default `50` (`core/src/config.rs`). Malformed env value falls through to file, not to hard 50 (`core/src/config.rs`). Example:

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

Verify: `RUNTIMO_OBSERVE_SAMPLE_HZ=bad runtimo observe --pid $$ --json` falls through to file (`core/src/config.rs` — `unverified` live, how to verify: run with bad env and inspect `hz` in JSON output).

### Safety contract

Collector `Halt` never kills the target — only the bundle watermark becomes `INCOMPLETE` (`core/src/observe/supervisor.rs`). All bounded channels (sampler 512, audit 512) drop newest and emit a `TRUNCATED` marker; never silent (`core/src/observe/sampler.rs`, `core/src/observe/audit.rs`). Secrets are redacted at every WAL boundary: any `target`, `location`, `frame`, or `error` containing `auth_token`/`bearer`/`api_key` (case-insensitive) is replaced with `REDACTED` before serialization (`core/src/observe/audit.rs`, `core/src/observe/sampler.rs`). Bundle hash chain detects tamper (`core/src/observe/bundle.rs`).

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
│   └── examples/            # ← moved from root to core/examples/
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
cargo test -p runtimo-core --lib    # 400 unit tests
cargo test -p runtimo-core --test integration  # 65 integration tests
cargo test -p runtimo-core --test robust       # property-based (proptest)
cargo test -p runtimo-core --doc    # doc tests
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
| `RUNTIMO_OBSERVE_SAMPLE_HZ` | `50` | Observe samples per second; precedence CLI `--sample-rate-hz` > env > `observe.sample_rate_hz` file > 50 (`core/src/config.rs`). Malformed env falls through to file. |
| `RUNTIMO_STATE_DIR` | `$XDG_DATA_HOME/runtimo` | Override state directory for WAL/backups/sessions |

## License

MIT — see [LICENSE](LICENSE).
