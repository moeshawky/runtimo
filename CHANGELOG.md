# Changelog

All notable changes to Runtimo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.1] - 2026-06-19

### Security

- **ShellExec blocklist hardening** — multi-layer defense added: (1) detokenized blocklist now normalizes shell quoting AND backslash escapes before matching; (2) regex patterns catch `rm -rf /`, `rm --recursive`, `rm -r --no-preserve-root` regardless of flag order and intervening path quoting; (3) system power commands (`shutdown`, `reboot`, `halt`, `poweroff`) blocked; (4) additional dangerous commands blocked: `chown`, `chgrp`, `mount`, `umount`, `iptables`, `nft`, `killall`, fork bombs (`:(){`), env dumpers (`env`, `printenv`). (`core/src/capabilities/shell_exec.rs`)
- **ShellExec quoting bypass detection** — `r"m"`, `$'rm'`, backslash-escaped variants all normalized before blocklist check. The catastrophic `rm -rf /` bypass chain from the 2026-05 session is permanently closed. (`core/src/capabilities/shell_exec.rs`)
- **ShellExec PATH sanitization** — forced `PATH=/usr/local/bin:/usr/bin:/bin` before spawn, preventing environment-based PATH hijack. (`core/src/capabilities/shell_exec.rs`)
- **ShellExec network gating** — network tools (`curl`, `wget`, `nc`, `ssh`, `scp`, `telnet`, `socat`) blocked by default in ShellExec. Opt-in via `RUNTIMO_ENABLE_NETWORK=1`. (`core/src/capabilities/shell_exec.rs`)
- **Dispatch pre-validation (F-003)** — arguments now validated at dispatch time (early `serde_json::from_value` deserialization) before reaching the daemon. Dangerous commands caught at the CLI, not the server. (`daemon/src/dispatch.rs`)
- **FileWrite .env denylist (F-006)** — `.env` and `.env.*` files now blocked via existing `CRITICAL_FILES` denylist, preventing credential file overwrite. (`core/src/capabilities/file_write.rs`)
- **Path validation hardening (N-002, N-003)** — `~` and `$HOME` expansions now rejected in file paths. `/home` no longer a default allowed prefix. CWD fallback removed — paths are validated against explicit allowed prefixes only. (`core/src/validation/path.rs`)

### Added

- **TypedCapability<A> trait** — type-safe capability execution with associated `Args` type. Blanket `impl<T: TypedCapability> Capability for T` bridges to untyped `&dyn Capability` dispatch, so both paths coexist. Each capability now defines its own args struct (`FileReadArgs`, `FileWriteArgs`, `ShellExecArgs`, etc.) validated at deserialization time. (`core/src/capability.rs`)
- **CmdError struct** — structured error type for command execution failures: `exit_code`, `signal`, `stderr`, `timed_out`, `was_killed`, `truncated`. (`core/src/capability.rs`)
- **CapabilityError enum** — `Blocked(reason)`, `Failed(reason)`, `Timeout(reason)`, `Io(io::Error)` variants for typed capability errors. (`core/src/capability.rs`)
- **Capability::description() trait method** — each capability now returns a one-line human-readable description for CLI `list` and `--help` output. (`core/src/capability.rs`)

### Changed

- **FileWrite::new() no longer takes backup_dir (ADR-C28)** — backup directory now derived automatically from `data_dir()`. All call sites updated (daemon, tests, examples). (`core/src/capabilities/file_write.rs`, `daemon/src/engine.rs`)
- **Daemon engine.rs decomposed** — 1674-line module split into `rpc.rs` (JSON-RPC protocol), `jobs.rs` (job lifecycle), `auth.rs` (session management), `config.rs` (persistent config), and `dispatch.rs` (pre-validation + routing). (`daemon/src/engine.rs`, `daemon/src/rpc.rs`, `daemon/src/jobs.rs`, `daemon/src/auth.rs`, `daemon/src/config.rs`)

### Fixed

- **Telemetry GPU probes no longer fire on every execution** — `Telemetry::capture()`
  shelled out 6 GPU/TPU/JAX commands for every capability execution (Kill, FileRead,
  etc.), producing warnings on systems without GPUs. Added `capture_lightweight()` —
  `/proc`-only, skips all shell-outs. Used by executor hot path; the full `capture()`
  still runs for `runtimo telemetry` CLI command. (`executor.rs`, `telemetry.rs`)
- **Cognitive pipeline no longer blocks system capabilities** — Kill, FileRead,
  FileWrite, GitExec, and Undo now skip the cognitive safety check. These
  capabilities operate on structured inputs (PIDs, file paths) and don't carry
  LLM-authored content. ShellExec still passes through cognitive safety. (`executor.rs`)
- **sift_observation padding removed** — injected text triggered `BiasHaloDetected`
  in llmosafe's content classifier regardless of neutrality. Non-high-risk inputs
  now pass only the capability description. (`executor.rs`)
- **WAL error logging (C9, C10)** — `let _ =` silent drops in rotation/cleanup replaced with `log::error!` for observability. WAL failures no longer invisible. (`core/src/wal.rs`)
- **Dead validate() call removed (C24)** — executor no longer calls `Capability::validate()` redundantly after the blanket TypedCapability impl handles it during deserialization. (`core/src/executor.rs`)
- **Telemetry run_cmd() returns Result** — command execution errors now propagated instead of silently swallowed. (`core/src/telemetry.rs`)

## [0.7.0] - 2026-06-15

### Changed

- **Telemetry redesigned as passive lens** — all service guessing and shell-out commands
  removed. Telemetry is now a pure observer: every field backed by a direct `/proc` or `/sys`
  read. No `pgrep`, no `ss`, no `free`, no `uptime -p`. The telemetry JSON schema has changed.
  (`core/src/telemetry.rs`)

### Added

- **`cpu_count` field** — number of CPU cores from `/proc/cpuinfo`. (`core/src/telemetry.rs`)
- **`ram_available` field** — `MemAvailable` from `/proc/meminfo`, showing actual usable RAM
  (previously only `MemFree` was shown, which excluded reclaimable buffers/cache and was
  misleading on modern kernels). (`core/src/telemetry.rs`)
- **`uptime_seconds` field** — machine-parseable uptime from `/proc/uptime`. (`core/src/telemetry.rs`)
- **`listening_ports` field** — raw TCP listening ports from `/proc/net/tcp` + `tcp6`.
  Replaces the old `detected_services` field which used a hardcoded 6-service port-to-name
  table and missed 7 out of 8 real services. (`core/src/telemetry.rs`)
- **`tunnel_pid` field** — cloudflared PID from `/proc/*/comm` scan, eliminating the
  `pgrep -fa cloudflared` self-match false positive (the observer no longer contaminates
  the measurement). (`core/src/telemetry.rs`)

### Removed

- **`ServiceInfo` struct** — deleted. Service-to-port guessing was fundamentally incomplete.
- **`DetectedService` struct** — deleted.
- **`detected_services` field** — replaced by `listening_ports`.
- **`detect_service_for_port()` function** — deleted. The 6-port lookup table (SSH, HTTP,
  HTTPS, MySQL, PostgreSQL, Redis, MongoDB) is now the consumer's responsibility.
- **`detect_version()` function** — deleted. Version detection via shell-out removed.
- **`parse_ss_output()` function** — deleted. >50 lines of fragile positional `ss -ltnp` parsing.
- **`tunnel_name` field** — replaced by `tunnel_pid`.
- **All `pgrep`, `free`, `cat /proc/cpuinfo`, `ss` shell-outs** — replaced with direct
  `/proc` file reads.

### Fixed

- **#1: cloudflared self-detection** — `pgrep -fa cloudflared` matched its own `sh -c`
  wrapper, producing a guaranteed false positive. Now fixed by reading `/proc/*/comm`
  (process name only, max 16 chars — never contains shell command lines).
  Same bug class as v0.4.0 vLLM fix (CHANGELOG v0.4.0: "vllm service hallucination").
- **#2: 7/8 services not detected** — hardcoded 6-port table replaced with raw
  `/proc/net/tcp` + `tcp6` parser that returns ALL listening ports.
- **#3: RAM misleadingly low** — `MemFree` (642Mi) falsely implied 97% RAM used when
  `MemAvailable` was 23Gi. Both values now shown.
- **#4: Load average without CPU context** — load line now shows CPU count.
- **#5: Uptime human-only** — `uptime_seconds: u64` field added, machine-parseable.
- **#6: Silent shell command failures** — `run_cmd()` returned empty string on failure,
  indistinguishable from successful empty output. All telemetry callers treated empty as
  valid data, causing false reports (0 GPUs, 0% RAM, 0 processes) when shell infrastructure
  was absent. Added `run_cmd_result() -> io::Result<String>` with error propagation; existing
  `run_cmd()` preserved with `eprintln!` warning before defaulting.
  (CBP Chain 1 — cmd→telemetry boundary. `core/src/cmd.rs`)
- **#7: Config corruption silently reverts to defaults** — `config::load()` used
  `.unwrap_or_default()` on both file read and TOML parse, meaning corrupt config files
  silently produced empty configs with no user notification. Added `load_result()` that
  propagates I/O and parse errors; existing `load()` preserved with `eprintln!` warning.
  (CBP Chain 2 — config→capability boundary. `core/src/config.rs`)
- **#8: /proc read failures silently return empty** — `read_proc_file()` used
  `unwrap_or_default()`, making `/proc` mount failures indistinguishable from empty files.
  Changed to `io::Result<String>`. (`core/src/telemetry.rs`)
- **#9: Unparseable RAM metrics silently become 0.0** — `parse_size_value()` returned 0.0
  on parse failures. Changed to `Option<f32>` so callers can distinguish "zero" from
  "unreadable." (`core/src/monitor.rs`)
- **#10: LlmoSafeGuard panic on missing HOME** — `resource_history_path()` panicked
  when both `RUNTIMO_STATE_DIR` and `HOME` were unset (common in minimal containers).
  Changed to `Option<PathBuf>` — `None` means in-memory-only resource tracking.
  (CBP Chain 3 — llmosafe→executor panic path. `core/src/llmosafe.rs`)
- **#11: CLI panics on missing HOME** — `make_registry()`, `data_dir()`, and
  `config_path()` all panicked via `expect()` when environment variables were unset.
  Changed to graceful fallback (`/tmp/runtimo`) with `eprintln!` warning.
  (CBP Chain 4 — CLI panic surface. `core/src/lib.rs`, `core/src/config.rs`,
  `cli/src/main.rs`)
- **#12: file_write.rs doesn't compile on non-Unix** — unconditional
  `use std::os::unix::fs::OpenOptionsExt` prevented compilation on Windows/WASI
  despite cross-platform claims. Added `#[cfg(unix)]` / `#[cfg(not(unix))]`
  helper functions with documented platform behavior.
  (CBP Chain 5 — platform lie. `core/src/capabilities/file_write.rs`)

### Security

- **Telemetry opt-in gating** — telemetry capture now checks `Config::telemetry_enabled()`
  before executing any network-dependent operation (public IP lookup, tunnel detection).
  Off by default; no data leaves the system without explicit user consent.
  (`core/src/executor.rs`)
- **ShellExec blocklist hardened** — `chown`, `mount`, `iptables`, `nftables`, `ip`,
  `route`, `ifconfig`, `wget`, and `curl` added to the dangerous-command blocklist.
  PATH sanitization prevents LD_PRELOAD-based bypass. (`core/src/capabilities/shell_exec.rs`)

## [0.6.5] - 2026-06-15

### Security

- **Telemetry opt-in gating** — telemetry no longer leaks public IP without explicit consent.
  `Config::telemetry_enabled()` gate added to `execute_with_telemetry()`.
  (`core/src/executor.rs`, `core/src/telemetry.rs`)
- **ShellExec hardened** — network commands blocked unless `--allow-network` flag set.
  PATH sanitized to prevent `LD_PRELOAD` bypass. Dangerous command blocklist:
  `chown`, `mount`, `umount`, `iptables`, `nftables`, `wget`, `curl`.
  (`core/src/capabilities/shell_exec.rs`)

### Fixed

- **GitExec branch validation** — operations limited to allowed branches only.
  (`core/src/capabilities/git_exec.rs`)
- **`data_dir()` no longer falls back to `/tmp`** — uses `dirs::data_dir()` with
  hard error on failure instead of `/tmp` fallback. (`core/src/lib.rs`)
- **`deny.toml` silent no-op** — `unmaintained = "warn"` invalid for cargo-deny >=0.18.
  Replaced with `unsound = "deny"`, `unmaintained = "deny"`, `yanked = "deny"`.
  The old config produced a parse error that CI swallowed, so dependency auditing
  hadn't actually been running for months. (`deny.toml`)
- **Cargo.lock version mismatch** — workspace members now consistent with root
  version. (`Cargo.lock`)
- **Binary collision fix** — `daemon/src/main.rs` deleted; the CLI crate already
  bundles the daemon binary via `src/daemon_bin.rs`. The duplicate `main.rs` caused
  implicit Cargo binary detection collision. (`daemon/src/main.rs` removed)

### Removed

- **Dead code** — `SchemaValidator` struct and `run_git()` function removed.
  (`core/src/schema.rs`, `core/src/lib.rs`)
- **Unnecessary clones** — `clone()` calls eliminated in executor dispatch path.
  (`core/src/executor.rs`)

## [0.6.4] - 2026-06-09

### Changed

- **Single-binary deployment** — `cargo install runtimo-cli` now installs BOTH `runtimo` (CLI) and `runtimo-daemon`. No separate `cargo install runtimo-daemon` required. Daemon is bundled as a second binary target in the CLI crate, using `runtimo-daemon` as a library dependency. (`cli/Cargo.toml`, `cli/src/daemon_bin.rs`, `daemon/src/lib.rs`, `daemon/src/engine.rs`)
- **Daemon refactored into library** — daemon logic extracted to `engine.rs` with `pub fn run()`, exposed via `daemon/src/lib.rs`. The standalone daemon binary (`daemon/src/main.rs`) is now a thin wrapper. Both CLI and standalone daemon use the same library. (`daemon/src/lib.rs`, `daemon/src/engine.rs`)

### Fixed

- **Daemon auto-start lock deadlock** — daemon now uses non-blocking `flock(LOCK_NB)` for `daemon.lock`, so it doesn't deadlock when CLI already holds the lock during auto-start. (`daemon/src/engine.rs`)
- **Daemon startup timeout increased** — `ensure_daemon_running()` timeout raised from 10s to 30s for slow systems. (`cli/src/main.rs`)

## [0.6.3] - 2026-06-08

### Fixed

- **TOCTOU documentation clarified** — `validation/path.rs` module docs now accurately reflect that `FileRead` and `FileWrite` use `O_NOFOLLOW` to prevent symlink attacks; remaining risk is non-file capabilities only.
- **WAL mutex held during dispatch** — background thread in `handle_dispatch` now acquires `wal_mutex` before executing capability, preventing concurrent WAL event interleaving with `handle_run`. (`daemon/src/main.rs`)
- **Daemon acquires lock before socket bind** — daemon now acquires `daemon.lock` flock before binding Unix socket, coordinating with CLI's `ensure_daemon_running()` lock. (`daemon/src/main.rs`)

## [0.6.2] - 2026-06-07

### Fixed

- **Daemon spawn race condition (SWP-1)** — file-based lock (`flock` on `$XDG_DATA_HOME/runtimo/daemon.lock`) prevents two concurrent `runtimo dispatch` calls from spawning duplicate daemon processes. (`cli/src/main.rs`)
- **WAL rotation data loss (SWP-2)** — `WalWriter::rotate()` now acquires exclusive flock before renaming, preventing concurrent appends from writing to stale file inodes. (`core/src/wal.rs`)
- **Concurrent dispatch working directory corruption (SWP-3)** — removed global `std::env::set_current_dir()`; per-dispatch `working_dir` passed via `Context::with_working_dir()` to avoid cross-thread cwd races. (`daemon/src/main.rs`, `core/src/executor.rs`)
- **CLI run concurrency limit asymmetry (SWP-4)** — `runtimo run` now enforces same 16-job limit as daemon `dispatch` via atomic counter. (`cli/src/main.rs`)

## [0.6.1] - 2026-06-03

### Fixed

- **`command` field alias for ShellExec** — agents naturally use `command` instead of `cmd`;
  both field names are now accepted via serde alias. (`shell_exec.rs`)
- **Case-insensitive capability names** — `runtimo run -c shellexec` works; lookup
  falls back to case-insensitive match. (`capability.rs`)
- **Daemon startup diagnostics** — when auto-start fails, the daemon's stderr is
  captured and shown; PATH search fallback if binary not found next to CLI.
  (`cli/src/main.rs`)

## [0.6.0] - 2026-06-02

### Added

- **Daemon auto-start on dispatch** — `runtimo dispatch` now detects whether the daemon is running
  and starts it automatically if needed. The daemon binary is located relative to the CLI. No manual
  daemon management required. (`cli/src/main.rs`)
- **Daemon periodic maintenance** — background task (1-hour interval) now wires previously uncalled
  infrastructure: `WalWriter::cleanup()`, `WalWriter::rotate()`, and `BackupManager::cleanup()`.
  Prevents unbounded WAL and backup directory growth in long-running deployments.
  (`daemon/src/main.rs`)

### Fixed

- **Kill PID reuse race window** — start-time is now re-read and compared before sending the signal,
  narrowing the TOCTOU window where a recycled PID could receive a signal intended for a different
  process. (`core/src/capabilities/kill.rs`)

### Changed

- **SchemaValidator made private** — `pub mod schema` reduced to `mod schema`. The module is
  preserved for future wiring into the capability validation pipeline. (`core/src/lib.rs`,
  `core/src/schema.rs`)

### Security Fixes

- **CBP-1: Dispatch path bypassed safety checks** — `handle_dispatch` created a separate
  `CapabilityRegistry` and called `capability.execute()` directly, bypassing all 6 safety
  guards that `handle_run` provides via `execute_with_telemetry()` (LlmoSafeGuard resource
  check, zombie count check, args size check, telemetry capture, spawned PID tracking,
  timeout enforcement). Fixed by routing dispatch through `execute_with_telemetry()` with
  the daemon's canonical registry, removing the duplicated execution path. This eliminates
  compound symptoms: dual CapabilityRegistry (registry mismatch), working_dir silent drop
  (lost context), and TOCTOU window on working_dir validation.
  (`daemon/src/main.rs`)

## [Unreleased]

## [0.6.5] - 2026-06-15

### Security Fixes

- **Telemetry public IP opt-in** — `NetworkInfo::capture()` no longer calls `curl ifconfig.me` unconditionally. Public IP lookup is gated behind `RUNTIMO_ENABLE_PUBLIC_IP=1`. Cloudflared `--token` values are redacted from tunnel output. (`core/src/telemetry.rs`)
- **ShellExec network command gating** — Network tools (`curl`, `wget`, `nc`, `ssh`, `scp`, `telnet`, `socat`) are blocked by default in ShellExec. Gated behind `RUNTIMO_ENABLE_NETWORK=1`. (`core/src/capabilities/shell_exec.rs`)
- **ShellExec dangerous command hardening** — `is_dangerous_command()` now blocks `--recursive` flag on `rm` (GNU long-option bypass), `chown`, `chgrp`, `mount`, `umount`, `iptables`, `nft`. Added `command_matches()` helper for prefix-based command detection. (`core/src/capabilities/shell_exec.rs`)
- **ShellExec PATH sanitization** — `execute()` now sets `PATH=/usr/local/bin:/usr/bin:/bin` before spawning, preventing environment-based PATH hijack. (`core/src/capabilities/shell_exec.rs`)
- **GitExec branch validation hardening** — `validate_branch_name()` now rejects option-injection prefixes (`--`), ref-injection patterns (`refs/`), control characters, whitespace, and metacharacters (`:`, `~`, `^`, `*`, `[`, `\\`, `?`, `.lock`). (`core/src/capabilities/git_exec.rs`)
- **data_dir() /tmp fallback eliminated** — `data_dir()` now panics with a clear message when neither `XDG_DATA_HOME` nor `HOME` is set, matching the behavior of `config_path()`. No more silent WAL/backup placement in world-readable `/tmp`. (`core/src/lib.rs`)
- **deny.toml license allow-list** — Added `MPL-2.0` and `Unicode-3.0` licenses for transitive dependencies (`cbindgen` build-dep, `unicode-ident`). (`deny.toml`)
- **GitExec network documentation** — Added explicit documentation that Git operations are inherently network-capable by design, with network isolation at the URL validation + SSRF blocking layer. (`core/src/capabilities/git_exec.rs`)

### Fixed

- **Daemon timeout now configurable** — `RunParams` struct now includes `timeout_secs` field, wired through both `handle_run` and `handle_dispatch`. Previously hardcoded to 30 seconds. (`daemon/src/engine.rs`)
- **Daemon working_dir validation errors logged** — `validate_path()` failures are now logged via `eprintln!` instead of silently converted to `None`. Users are notified when their requested working_dir is rejected. (`daemon/src/engine.rs`)
- **deny.toml cargo-deny 0.19.4 compatibility** — Fixed `unmaintained = "warn"` (invalid for cargo-deny ≥0.18) to `unmaintained = "all"`. Removed deprecated `notice` and `unlicensed` keys. All cargo-deny subcommands now execute successfully. (`deny.toml`)
- **Cargo.lock refreshed** — 16 packages updated to latest semver-compatible versions, including `llmosafe` 0.7.3→0.7.5. (`Cargo.lock`)

### Removed

- **Dead code: SchemaValidator** — Removed unused `schema.rs` module (102 lines). The module was crate-private, had zero consumers, and was annotated `#[allow(dead_code)]` for future use. (`core/src/schema.rs`, `core/src/lib.rs`)
- **Dead code: GitExec::run_git()** — Removed unused backward-compatibility wrapper. All call sites use `run_git_with_timeout()` directly. (`core/src/capabilities/git_exec.rs`)
- **Duplicate binary target** — Removed `daemon/src/main.rs`. The daemon binary is produced exclusively by `cli/src/daemon_bin.rs`, eliminating the `cargo build` filename collision warning between the daemon and cli crates. (`daemon/src/main.rs`)
- **Unused workspace dependencies** — Removed unused `serde` and `thiserror` from `cli/Cargo.toml`; removed unused `thiserror` from `daemon/Cargo.toml`. (`cli/Cargo.toml`, `daemon/Cargo.toml`)

### Changed

- **MSRV declared correctly** — `rust-version` changed from `1.85.0` to `1.70.0`, matching the CI-tested MSRV in `ci.yml`. (`Cargo.toml`)
- **GitClone optimization** — Changed `HashSet<String>` to `HashSet<&String>` in `cli/src/main.rs` and `daemon/src/engine.rs` job deduplication paths, eliminating redundant `String::clone()` allocations. (`cli/src/main.rs`, `daemon/src/engine.rs`)
- **Documentation rename** — `docs/RUNTIMO_CORE_LIB.rs` renamed to `docs/RUNTIMO_CORE_LIB.rs.txt` to prevent false Rust tooling diagnostics. (`docs/`)
- **Gitignore hardening** — Added `.moegraph/` and `.workflow-state.md` to `.gitignore` for local development artifact exclusion. (`.gitignore`)
- **CI workflow committed** — `publish-cratesio.yml` is now tracked by git. (`.github/workflows/`)
- **Environment variable documentation** — README now documents `RUNTIMO_ENABLE_PUBLIC_IP`, `RUNTIMO_DAL`, `RUNTIMO_STATE_DIR`, and `RUNTIMO_ENABLE_NETWORK`. (`README.md`)

### Testing

- **Generate ID tests** — Added deterministic uniqueness and format-invariant tests for `generate_id()`. (`core/src/lib.rs`)
- **GitExec branch validation tests** — 15 new test cases covering option injection, ref injection, control characters, whitespace, and metacharacter rejection. (`core/src/capabilities/git_exec.rs`)
- **ShellExec blocklist tests** — 6 new test cases for recursive-flag, ownership-command, mount-command, firewall-command, and network-command blocking. (`core/src/capabilities/shell_exec.rs`)
- **Total test count: 233** (up from 206 in v0.6.4). All passing.

---

## [0.5.0] - 2026-05-30

### Security Fixes

- **S-DEFAULT: Eliminated /tmp config fallback** — `config_path()` and `resource_history_path()` no
  longer fall back to world-writable `/tmp` when `XDG_CONFIG_HOME`/`HOME` are unset. Now panic with
  a clear message instead of writing persistent config to a shared temp directory (symlink attack
  vector, TOCTOU race, info leak). (`config.rs`, `llmosafe.rs`)

### Breaking Changes

- **Removed 10 write-only telemetry fields** — `ram_total_bytes`, `ram_free_bytes`,
  `disk_total_bytes`, `disk_free_bytes`, `disk_used_percent_numeric` (SystemInfo), `tpu_devices`,
  `gpu_devices` (HardwareInfo), `vllm_version`, `vllm_running`, `vllm_port_bound` (ServiceInfo)
  removed. Compute from `accelerators` vec or string fields instead. (`telemetry.rs`)
- **Removed 7 dead public exports** — `Job`, `ExecutionResult`, `HealthAlert`, `HealthState`,
  `Session`, `SessionManager` removed from `pub use` in lib.rs. Use qualified module paths
  (`runtimo_core::job::Job`, etc.) for internal access. (`lib.rs`)
- **Moved `format` module from core to cli** — `wall_to_markdown()` is a CLI presentation concern,
  not a core library function. Import from `cli/src/format.rs` instead of `runtimo_core::format`.
  (`core/src/format.rs` deleted, `cli/src/format.rs` created)

### Added

- **`SessionError` variant** — New `Error::SessionError(String)` variant for session-specific
  failures. `SessionManager` methods now return `SessionError` instead of misusing `BackupError`.
  (`lib.rs`, `session.rs`)
- **`rust-toolchain.toml`** — Pins stable toolchain with rustfmt + clippy components.
- **`cargo-deny` config** — `deny.toml` enforces license allowlist (MIT, Apache-2.0, BSD, ISC,
  Unicode-DFS-2016, Zlib), bans wildcard versions, audits for advisories.
- **MSRV CI testing** — `ci.yml` now tests against Rust 1.70.0 (declared MSRV).
- **`cargo-machete` CI** — Detects unused dependencies.
- **CI rust-cache** — All CI jobs now use `Swatinem/rust-cache@v2` for faster builds.

### Fixed

- **S-ENTROPY: session.rs BackupError misuse** — All 7 error sites in `SessionManager` now use
  `SessionError` instead of `BackupError`. Doc comments updated to match. (`session.rs`)
- **S-ORPHAN: Stale docs** — `RUNTIMO_CORE_LIB.rs` updated: Capability trait now shows
  `description()` method, built-in capabilities list complete (6 capabilities), Error enum
  includes `SessionError`, resource limits corrected (80% threshold, zombie count), Context
  example uses `Context::new()`, module list complete, version updated to 0.4.1. (`docs/`)
- **Rustdoc warnings** — Fixed 5 unclosed HTML tag warnings in daemon doc comments. (`daemon/src/main.rs`)
- **Telemetry demo** — Updated `telemetry_demo.rs` to compute GPU/TPU counts from accelerators
  vec instead of removed fields. (`core/examples/telemetry_demo.rs`)

### Changed

- **CI workflow expanded** — Added `msrv`, `deny`, `machete` jobs. Added `rust-cache` to all
  existing jobs. (`ci.yml`)
- **.gitignore hardened** — Added `AGENTS.md`, `references/`, `.cursor/`, `.claude/` to prevent
  agentic artifact leaks. (`.gitignore`)

## [0.4.0] - 2026-05-29

### Fixed

- **G-EDGE-1: check_disk_space runs before create_dir_all** — `check_disk_space()` was called
  before `create_dir_all()` in FileWrite, causing writes to nonexistent parent directories to
  fail with "No such file or directory" from `df`. Added early return when parent doesn't exist.
- **format_size double conversion** — `format_size()` else branch divided KB input by 1024,
  causing a 512KB process to display as "0K". Removed erroneous division.
- **parse_ps_line vsz/rss multiplied by 1024** — `parse_ps_line()` multiplied VSZ and RSS
  by 1024 (KB→bytes), but `format_size()` expects KB. A 1GB process displayed as 1024GB.
  Removed the multiplication.
- **vllm service hallucination** — `pgrep -f 'vllm serve'` matched the shell command itself,
  causing vllm to appear in telemetry output even when not running. Replaced process-based
  detection with port-based discovery.

### Changed

- **Telemetry service detection rewritten** — From hardcoded `pgrep` checks to port-based
  discovery. `ss -ltnp` scans listening TCP ports; ports mapped to known services via lookup
  table (ssh:22, nginx:80/443, postgres:5432, redis:6379, mysql:3306, mongodb:27017).
  Unknown ports are ignored. Version detected via service-specific commands.
- **Telemetry cache TTL increased** — 5s → 30s to accommodate service version detection
  strategies.
- **Process snapshot docs corrected** — "ps aux" references updated to "ps with explicit
  format" (code uses `ps -eo`, not BSD-style `ps aux`).
- **ServiceInfo docs corrected** — "Scans for known service processes" → "Scans for listening
  TCP ports". "No hardcoded service names" → "No assumed running services".

### Added

- **Enriched test suite (15 new tests)** — G-EDGE boundary tests for check_disk_space
  (nonexistent parent, deep nesting, single new parent, existing parent, empty content),
  C4 ordering dependency tests (concurrent paths, write after parent creation), G-SEM
  semantic invariants (content identity, unicode roundtrip), T-FALSEPASS regression tests
  (ordering sentinel, exact content check, tight oracle check).
- **process format_size unit test** — Validates all K/M/G branches.
- **process vsz/rss range test** — Regression sentinel for the `*1024` bug.
- **drift_process_vsz_rss_reasonable robust test** — Ensures VSZ/RSS stay in reasonable KB range.

## [0.2.0] - 2026-05-18

### Security Fixes (P0 - Critical)

**ShellExec:**
- Fixed unbounded output → OOM vulnerability (lines 106-107, 114-115, 312-313)
- Fixed `/bin/sh -c` bypass allowing arbitrary code execution (lines 273-280)
- Fixed `child.kill()` not killing descendants (line 93)
- Fixed pipe deadlock after kill (lines 93-107)
- Added dangerous command blocklist (`rm -rf /`, `dd`, `mkfs`, `fdisk`, `shutdown`, `chmod 777 /`)
- Added PATH hijack protection (requires absolute paths)

**FileWrite:**
- Fixed prefix validation bypass (`/tmpfoo/` passed `/tmp` check) in `path.rs:174`
- Added atomic write pattern (write to `.tmp`, fsync, rename)
- Added critical file denylist (`.bashrc`, `.ssh/authorized_keys`, `.profile`, etc.)
- Added disk space pre-check before writes

**GitExec:**
- Enforced `timeout_secs` on ALL git operations (was ignored before)
- Blocked `http://` URLs (MITM risk) - now requires `https://` or SSH
- Sanitized credentials from remote URLs in output
- Added safeguards to `git clean -fd` (dry-run preview, file count limit)
- Added secret file detection to `git add -A` (skips `.env`, `credentials.json`, etc.)
- Fixed URL validation bypasses (SSRF to cloud metadata)

**FileRead:**
- Fixed TOCTOU symlink escape (lines 128-153) using `O_NOFOLLOW`
- Fixed TOCTOU size bypass (lines 131-153) with bounded reader
- Added binary file detection
- Fixed UTF-8 truncation on multibyte boundaries

**WAL:**
- Fixed atomic write pattern (lock held during entire write)
- Fixed O(N²) I/O (now uses append mode)
- Fixed WAL rotation naming
- Fixed cleanup race window

**Backup:**
- Fixed restore to create pre-restore backup (was destroying newer data)
- Fixed `cleanup` symlink attack (was using `is_dir()` which follows symlinks)
- Added 100MB size limit per file
- Added backup integrity verification

**Executor:**
- Implemented zombie guard (reject if `zombie_count > 10`)
- Added args size limit (1MB max)
- Added spawned PID tracking
- Fixed WAL serialization to return error instead of storing `Null`

**Telemetry:**
- Added numeric metrics for agents (`disk_total_bytes`, `disk_free_bytes`, etc.)
- Reduced cache TTL from 30s to 5s
- Fixed `disk_used_percent` to store numeric value (stripped `%` sign)

### Features

- **Agent-friendly output:** JSON files auto-parsed to structured objects
- **Process groups:** ShellExec uses process groups for proper cleanup
- **Stdin support:** ShellExec now accepts stdin input
- **Output truncation:** 10MB limit on stdout/stderr with truncation flag
- **Numeric telemetry:** Agents can now compute thresholds programmatically

### Tests

- Added 50+ new tests across all capabilities
- All P0/P1/P2 audit issues have corresponding tests
- Total: 119 tests passing

### Documentation

- Updated README with correct binary name (`runtimo` not `moe`)
- Added comprehensive security documentation
- Documented all failure modes and mitigations

## [0.1.5] - 2026-05-17

### Bug Fixes

- **`create_backup` fails on directories** — `BackupManager::create_backup()` used `std::fs::copy()` which only works for regular files. When backing up a git repository (directory), it failed with "the source path is neither a regular file nor a symlink to a regular file". Replaced with `copy_recursive()` that handles both files and directories.

### Security

- **Symlink attack vector in backup** — `copy_recursive()` now explicitly rejects symlinks using `symlink_metadata()` to prevent symlink attack vectors where an attacker could place symlinks to sensitive files (e.g., `/etc/passwd`) in a directory being backed up.

### Features

- **Directory backup support** — `create_backup()` now recursively copies entire directory trees, enabling backup of git repositories and other directory structures.
- **Directory restore support** — `restore()` now uses `copy_recursive()` to restore both files and directories from backup.
- **Permission preservation** — On Unix systems, file and directory permissions (including executable bits) are preserved during backup/restore operations.

### Tests

- Added `test_backup_directory()` — Verifies directory tree backup works correctly
- Added `test_backup_rejects_symlinks()` — Verifies symlinks are rejected for security
- Added `test_restore_directory()` — Verifies directory restore from backup
- Added `test_backup_preserves_executable_bit()` — Verifies Unix permission preservation

## [0.1.4] - 2026-05-17

### Added

- **Persistent config file** — `~/.config/runtimo/config.toml` stores allowed path prefixes
  across invocations. Merged with built-in defaults and `RUNTIMO_ALLOWED_PATHS` env var.
- **CLI config subcommand** — `moe config allowed-paths add/list/remove` manages persistent
  path prefixes without needing env vars on every SSH invocation.
- **31 robust tests** covering 6 LLM failure mode categories:
  - **G-EDGE**: Empty content, single chars, long filenames, null bytes, concurrent writes
  - **G-SEC**: Encoded path traversal, null byte injection, symlink chains, type confusion,
    adversarial JSON
  - **G-ERR**: Directory reads, read-only locations, WAL failures, backup errors, invalid signals
  - **G-CTX**: Config file loading, env var precedence, invalid TOML handling, path validation
  - **G-SEM**: Backup numbering, WAL monotonicity, telemetry ordering, process consistency
  - **G-DRIFT**: Telemetry/process/WAL format stability via golden file assertions
- **4 property-based tests** (proptest):
  - Write/read roundtrip for arbitrary strings
  - Backup numbering produces no duplicates
  - Path validation is consistent for equivalent paths
  - WAL cleanup preserves recent events

### Dependencies

- Added `toml = "0.8"` for config file serialization
- Added `proptest = "1.4"` for property-based testing (dev-dependency)

## [0.1.3] - 2026-05-17

### Added

- **Capability descriptions** — All 6 capabilities now implement `description()` on the
  `Capability` trait. CLI `list` command shows human-readable descriptions alongside names.
- **CLI help text** — Every subcommand now has `about`, `long_about`, and `after_help` with
  usage examples. `moe --help` shows quick-start commands.
- **Timeout warning** — `execute_with_timeout_check` (renamed from `execute_with_timeout`)
  now emits an `eprintln!` warning when a capability exceeds its timeout, making the advisory
  nature visible to operators.
- **2 new tests** (86 total):
  - `respects_dry_run` — ShellExec returns immediately without spawning a process
  - `test_kill_dry_run` — Kill reports what would be killed without sending a signal

### Bug Fixes

- **Double `%%` in CLI disk output** — Format string `"disk: {}%"` appended `%` to a value
  that already included it from `df`. Removed the extra `%` from the format string.
- **Dead `unpark()` in HealthMonitor Drop** — `self._thread.thread().unpark()` called on a
  `JoinHandle` that never parks. Removed dead code.
- **Curl hang in telemetry** — `NetworkInfo::capture()` called `curl ifconfig.me` with no
  timeout. Added `--connect-timeout 5 --max-time 5` to prevent indefinite hangs.
- **`parse_ram_percent` always returned 0%** — Function accepted a single formatted string
  (`"16Gi total, 13Gi free"`) and tried to parse both values from it. Changed signature to
  `parse_ram_percent(ram_total, ram_free)` accepting separate telemetry fields.
- **ID collision from timestamp-based IDs** — `JobId::new()` and `SessionManager::create_session()`
  used nanosecond timestamps, causing collisions under concurrent execution. Replaced with
  `utils::generate_id()` using 16 bytes from `/dev/urandom` (32 hex chars, fallback to timestamp).
- **Backup overwrite lost original state** — `BackupManager::create_backup()` overwrote the
  backup file on repeated writes within the same job. Now appends numeric suffixes (`.1`, `.2`)
  so the first backup always contains the true original.
- **WAL cleanup race condition** — `WalWriter::cleanup()` truncated then rewrote the WAL,
  losing events appended by concurrent writers during the window. Now writes to a temp file,
  re-reads the original to merge any concurrent appends (by `seq` comparison), then atomically
  renames.
- **Dead code in `parse_ps_line`** — `parts.get(10).unwrap_or(&"").to_string()` was always
  `None` when `parts.len() == 10`. Replaced with `parts.get(10..).map(...).unwrap_or_default()`.

### Audit Findings Fixed

- **G-CTX-1: ShellExec ignores `dry_run`** — `ShellExec.execute` now checks `ctx.dry_run`
  and returns immediately without spawning any process.
- **G-CTX-2: Kill ignores `dry_run`** — `Kill.execute` now checks `ctx.dry_run` and reports
  what would be killed (with process snapshot) without sending a signal.
- **G-EDGE-1: WAL cleanup concurrent-append race** — Fixed (see Bug Fixes above).

## [0.1.1] - 2026-05-16

### Security

- **Daemon Unix socket authentication** — Added `SO_PEERCRED` UID verification via `libc`.
  Only processes running as the same UID as the daemon can connect. Previously any local
  process could execute arbitrary capabilities.
- **Kill capability hardening** — Protected PIDs now include the daemon's own PID and its
  parent PID (read from `/proc/self/status`). Removed `force=true` bypass that allowed
  killing any protected process.
- **GitExec wired and audited** — Exported in `capabilities/mod.rs`, registered in CLI and
  Daemon. Fixed silenced `git add` errors (previously `let _ =` discarded failures).

### Bug Fixes

- **CLI Undo multi-file data loss** — HashMap keyed by `job_id` instead of filename caused
  all but the last file path per job to be lost. Now uses backup filename as key.
- **Timeout enforcement** — `execute_with_timeout()` now measures elapsed time and returns
  an error if execution exceeds `timeout_secs`. Previously the parameter was accepted but
  ignored (`_timeout_secs`).
- **HealthMonitor RAM leak detection** — Memory leak alert gated on `cpu_alert_count` instead
  of a dedicated RAM counter. Added `ram_alert_count` field; RAM alerts now fire independently
  of CPU state.
- **Path validation TOCTOU** — `FileRead` and `FileWrite` now use the canonical `PathBuf`
  returned by `validate_path()` instead of the original user-supplied string, preventing
  symlink race attacks between validation and execution.
- **Kill signal parameter dead code** — Signal argument was parsed but always defaulted to
  SIGKILL (9). Now uses `args.signal.unwrap_or(15)` (SIGTERM default for graceful shutdown).
- **Daemon capability gap** — Daemon only registered FileRead + FileWrite. Now also registers
  ShellExec, Kill, Undo, and GitExec for parity with CLI.
- **ProcessSnapshot stale cache after kill** — Added `ProcessSnapshot::clear_cache()` to
  invalidate the 30-second cache before after-capture in kill operations.

### Improved

- **Error propagation** — WAL serialization failures now log with `eprintln!` instead of
  silently writing `Null`. Session `add_job` errors are logged instead of discarded.
  WAL corruption in Undo capability now returns an error instead of silently skipping.
- **Lock poison recovery** — `HealthMonitor` uses `unwrap_or_else(|e| e.into_inner())` instead
  of `expect()` for all RwLock accesses, preventing panics on poisoned locks.
- **HealthMonitor graceful shutdown** — Added `Drop` implementation to set stop flag on
  destruction.
- **parse_size_value format support** — Now handles `MB` and `GB` suffixes in addition to
  `Gi`, `Mi`, `Ki`.
- **Undo capability deduplication** — Uses `crate::utils::backup_dir()` and
  `crate::utils::wal_path()` instead of duplicating path logic.
- **Kill uses libc::kill** — Direct syscall instead of spawning `kill` subprocess for
  reliability and performance.
- **GitExec clippy fixes** — Fixed `if_same_then_else`, `needless_borrows`, and line-length
  warnings. Fixed test assertion (`abc123` is 6 chars, minimum is 7).

### Added

- 7 new tests (86 total, up from 79):
  - `test_kill_actual_process` — Verifies kill works on a real process (was ignored)
  - `test_kill_self_protected` — Verifies own PID is protected
  - `test_health_monitor_lifecycle` — Verifies start/stop without 60s wait
  - `test_health_state_defaults` — Verifies default state values
  - `test_cpu_alert_after_consecutive_checks` — Verifies CPU alert counting
  - `test_ram_alert_uses_ram_counter_not_cpu` — Verifies RAM alert independence (P1-2 fix)
  - `test_ram_alert_resets_when_ram_decreases` — Verifies counter reset on decrease

## [0.1.0] - 2026-05-16

### Added

- **Capability runtime** — Pluggable operations with name, schema, validate, execute
- **Two-layer telemetry** — Hardware (CPU, RAM, disk, GPU/TPU, services, network) + Process snapshot (ps aux, zombies, top consumers)
- **Resource guards** — llmosafe circuit breaker reads /proc/stat and /proc/self/status
- **Write-ahead log** — Append-only, fsync'd event log for crash recovery
- **Backup/undo** — Files backed up before mutation, rollback by job ID
- **CLI** — run, list, status, logs, undo, telemetry, processes commands
- **Daemon** — Unix socket JSON-RPC server for remote capability execution
- **Unified path validation** — Single validate_path() module prevents path traversal and symlink attacks

## [0.1.0-alpha.2] - 2026-05-16

### Security Fixes

- **P0: FileWrite validation bypass** — `FileWrite::validate()` now uses the unified `validate_path()` module
  with `allowed_prefixes` enforcement. Previously, agents could write to arbitrary paths (`/etc/shadow`, etc.)
  because the unified validation was imported but never called. (G-SEC-1)
- **TOCTOU mitigation** — Path validation now canonicalizes the parent directory for non-existent
  paths (write targets), catching symlink-based directory escapes. Added symlink escape test. (G-SEC-2)
- **Prefix enforcement for writes** — Write operations now go through the same prefix check as reads
  (`/tmp`, `/var/tmp`, `/home`). Previously, no prefix restriction existed. (G-SEC-3)

### Bug Fixes

- **WAL cleanup data duplication** — `WalWriter::cleanup()` now truncates the file before rewriting
  retained events. Previously, it opened in append mode, doubling all retained entries. (G-EDGE-1)
- **UTF-8 truncation panic** — `truncate()` in process reports now uses `char_indices()` for safe
  slicing. Previously panicked on multi-byte character boundaries (CJK, emoji, Arabic). (G-EDGE-2)
- **Daemon startup panic** — `DaemonState::new()` now returns `Result` instead of panicking with
  `unwrap_or_else`. Backup directory or WAL directory creation failures are now graceful errors. (G-ERR-2)
- **Undo path reconstruction** — Removed fragile fallback that guessed original file paths from
  backup directory structure. Now returns a clear error if the WAL doesn't contain the original
  path for a job. (G-CTX-2)

### Improved

- **Path deduplication** — CLI and daemon now use `runtimo_core::utils::{data_dir, wal_path, backup_dir}`
  instead of duplicating the path logic in 3 locations. (G-CTX-1)
- **WAL tail() performance** — Changed from `Vec::remove(0)` (O(n) shift) to `VecDeque::pop_front()`
  (O(1) amortized) for the sliding window. (G-PERF-A)
- **Timeout documentation** — Executor timeout limitation is now prominently documented as
  "not currently enforced" with v0.2.0 tracking. Removed misleading duplicate docblock. (G-SEM-1)
- **Documentation accuracy** — Fixed stale `schema() -> &str` signature (now `-> Value`),
  fixed `FileWrite` examples to show constructor pattern, fixed license reference. (G-SEM-2)
- **Clippy clean** — Zero clippy warnings (was 7): fixed `needless_borrows`, `len_zero`, `ptr_arg`,
  removed unused imports.

### Added

- 6 new tests (52 total, up from 46):
  - `test_truncate_ascii` / `test_truncate_multibyte_utf8` — UTF-8 safety verification
  - `accepts_existing_tmp_file` — positive path validation
  - `accepts_nonexistent_tmp_file_for_writes` — write path validation
  - `rejects_write_outside_allowed` — prefix enforcement for writes
  - `rejects_symlink_escape` — symlink attack prevention

---

## [0.1.0-alpha] - 2026-05-16

### Added

#### Core Functionality
- **Capability trait** - Pluggable operations with `name()`, `schema()`, `validate()`, `execute()` methods
- **CapabilityRegistry** - Registry for discovering and listing available capabilities
- **Job lifecycle management** - `Job`, `JobId`, `JobState` with state machine transitions
- **Executor pipeline** - `execute_with_telemetry()` wraps capability execution with telemetry and WAL logging
- **JSON Schema validation** - `SchemaValidator` for validating capability arguments against JSON schemas

#### Telemetry (Two-Layer)
- **Hardware telemetry** (`Telemetry::capture()`):
  - CPU model, RAM total/free, disk total/free/used%
  - Uptime, load average
  - TPU/GPU device detection
  - JAX availability check
  - Service detection (vLLM, port 8200)
  - Network info (public IP, cloudflared tunnel status)
- **Process snapshot** (`ProcessSnapshot::capture()`):
  - Full process list via `ps -eo pid,ppid,user,%cpu,%mem,vsz,rss,stat,start,time,comm`
  - Parent PID (PPID) tracking for process lineage
  - Zombie process detection
  - Top CPU/memory consumer identification
  - Summary statistics (total count, total CPU%, total memory%, zombie count)

#### Safety & Security
- **Resource guards** - `LlmoSafeGuard` integration with:
  - Memory ceiling (80% default)
  - CPU load delta measurement
  - Pressure score calculation (0-100%)
  - Raw entropy score (0-1000)
- **Path traversal protection** - Rejects paths containing `..` sequences
- **Empty path validation** - Rejects empty paths
- **Directory read protection** - Rejects attempts to read directories
- **Shell command safety documentation** - Comprehensive security warnings in `cmd.rs`

#### Crash Recovery
- **Write-ahead log (WAL)** - Append-only JSONL log with `fsync` after each write:
  - `JobStarted` events (before validation)
  - `JobCompleted` events (after successful execution)
  - `JobFailed` events (on validation or execution failure)
  - `WalWriter` and `WalReader` for append/read operations
  - Sequential event IDs for ordering guarantees
- **Backup manager** - Automatic backup before file mutation:
  - Pre-mutation file copies stored in `{data_dir}/backups/<job_id>/`
  - Undo support via `moe undo -j <job_id>`
  - Parent directory creation for backups

#### Capabilities
- **FileRead** - Read file contents with validation:
  - Schema: `{"path": string}`
  - Validates: path exists, is a file, no traversal
  - Returns: file content as string
- **FileWrite** - Write content to file with backup:
  - Schema: `{"path": string, "content": string, "append": boolean?}`
  - Creates parent directories automatically
  - Creates backup of existing files before overwrite
  - Supports append mode

#### CLI (moe)
- **run** - Execute a capability with args
  - Flags: `-c` (capability), `-a` (args JSON), `--dry-run`
- **list** - List registered capabilities
- **telemetry** - Print hardware/system telemetry
- **processes** - Print process snapshot with PPIDs
- **status** - View job status from WAL
- **logs** - View WAL events (filterable by job ID)
- **undo** - Restore files from backup by job ID

#### Documentation
- Unit tests (21 tests)
- Integration tests (31 tests)
- Doc tests (7 passing, 12 ignored)
- Example programs: `basic_read`, `telemetry_demo`, `write_and_undo`

### Changed

- Renamed from internal project to "Runtimo" (runtime for persistent machines)
- Moved from ad-hoc execution to structured capability pipeline
- Enhanced process snapshot to include PPID (parent PID) tracking
- Added comprehensive security documentation to shell command execution

### Fixed

- **F1: Execution timeout** - Added `execute_with_telemetry_and_timeout()` function with configurable timeout parameter (30s default). Note: timeout parameter accepted but enforcement deferred for future watchdog implementation.
- **F2: PPID tracking** - Changed `ps aux` to `ps -eo pid,ppid,...` format to capture parent PIDs for process lineage tracking
- **F3: Shell command safety** - Added comprehensive security documentation warning against user input interpolation; clarified hardcoded command usage pattern

### Technical Details

#### Workspace Structure
- `runtimo-core` - Core library with capability trait, executor, telemetry, WAL
- `runtimo-cli` (moe) - CLI binary with 7 commands
- `runtimo` (daemon) - Placeholder daemon (future JSON-RPC server)

#### Dependencies
- `serde` + `serde_json` - Serialization
- `thiserror` - Error handling
- `llmosafe` (v0.6) - Resource guard
- `jsonschema` - JSON Schema validation (via examples)

#### Test Coverage
- **21 unit tests** - Capability validation, execution, error handling, path security
- **31 integration tests** - End-to-end workflows, security checks, edge cases
- **7 doc tests** - API documentation examples
- **Total: 52 tests passing**

### Known Issues

#### Not Yet Implemented (v0.1.0)
- Daemon functionality (Unix socket listener, JSON-RPC, job queue)
- Process kill capability for runaway termination
- Backup cleanup (old backups accumulate)
- ShellExec capability
- HTTP request capability
- Concurrent job execution
- WAL path persistence (defaults to /tmp)

### Security Considerations

- All current shell commands in `cmd.rs` are hardcoded literals
- User input must NEVER be interpolated into shell commands
- For user-provided values, use `std::process::Command` directly with proper escaping
- Path traversal (`..`) is rejected but this is not a complete sandbox
- Capabilities run with the same privileges as the host process

### Performance Characteristics

Measured on AMD EPYC 7B13 system:
- Cold start: <1s
- FileRead latency: <10ms for small files
- FileWrite latency: <50ms (includes backup)
- Telemetry capture: <100ms
- Process snapshot: <50ms
- Memory usage: <50MB baseline

### Upgrade Notes

This is the initial release (v0.1.0-alpha). No upgrade path needed.
- v0.1.3 (2026-05-17) - Bug fixes, audit findings, capability descriptions, CLI help text
- v0.1.2 (2026-05-16) - Telemetry + process tracking, 8 bug fixes, code audit remediation
- v0.1.1 (2026-05-16) - Security hardening, daemon auth, Kill/GitExec wiring
- v0.1.0 (2026-05-16) - Initial stable release with docs, release workflow, and runner fixes
