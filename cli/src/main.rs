//! runtimo CLI — Agent capability runtime with background dispatch

mod format;
mod output;
mod session_parser;
mod session_run;

use clap::{Parser, Subcommand};
use output::OutputMode;
use runtimo_core::{
    capabilities::{
        is_dangerous_command, is_network_command, network_enabled, Delete, FileRead, FileWrite,
        GitExec, Kill, ShellExec, Undo,
    },
    execute_with_telemetry_and_session, CapabilityRegistry, ProcessSnapshot, RuntimoConfig,
    Telemetry, WalReader,
};
use serde_json::Value;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Maximum seconds to wait for daemon to become ready after spawning.
const DAEMON_STARTUP_TIMEOUT_SECS: u64 = 30;

/// Maximum size for capability arguments in bytes (~130 KB).
const MAX_ARGS_SIZE_BYTES: usize = 130 * 1024;

#[derive(Parser)]
#[command(
    name = "runtimo",
    about = "capability runtime with telemetry, WAL, process tracking, and background dispatch",
    long_about = "runtimo — capability runtime with telemetry, WAL, and process tracking\n\n\
Every exec: telemetry + process snapshot + WAL audit\n\
Background: dispatch jobs to daemon, check status later",
    after_help = "USAGE:\n runtimo run -c <Capability> -a '<json>'\n runtimo dispatch -c <Capability> -a '<json>'\n runtimo jobs\n runtimo wait -j <job_id>\n runtimo list\n runtimo logs\n runtimo telemetry\n runtimo processes\n\nCAPABILITIES:\n FileRead  Read file. Path validated (allowed dirs only). No dirs, no traversal.\n FileWrite Write file. Auto-backup for undo. Append mode ok.\n Delete    Delete a file. Auto-backup for undo unless no_backup=true. Path-validated (no rm bypass).\n ShellExec Exec via sh -c. Blocks many dangerous commands (see `runtimo list` for full blocklist). Network tools and interpreters are opt-in.\n GitExec   Git ops: clone|pull|commit|revert|clean|status.\n Kill      Kill process by PID. Protected: init, kthreadd, self, parent, session/group leaders, systemd services.\n Undo      Restore from backup. Find job IDs with `runtimo jobs` or `runtimo logs`.\n\nTIP: Use `runtimo run -c <Cap> --schema` to see the JSON args a capability expects.\nTIP: Use `runtimo list --schemas` to see all schemas at once.\nTIP: ShellExec timeout has no upper bound (default: 30).\n\nDaemon starts on first dispatch if runtimo-daemon is installed.",
    version
)]
#[allow(clippy::struct_excessive_bools)] // 6 bools map to 3 orthogonal flag pairs (color/no_color, emoji/no_emoji, timestamps/no_timestamps); enum refactor would churn CLI without safety gain
struct Cli {
    /// Output format: human|json|plain|quiet
    #[arg(long, global = true, value_name = "FORMAT")]
    output: Option<String>,
    /// Enable ANSI color (requires tty, honored only when explicitly set; --no-color wins, NO_COLOR env forces off)
    #[arg(long, global = true)]
    color: bool,
    /// Disable ANSI color (wins over --color and NO_COLOR)
    #[arg(long, global = true)]
    no_color: bool,
    /// Enable emoji (off by default; --no-emoji wins)
    #[arg(long, global = true)]
    emoji: bool,
    /// Disable emoji
    #[arg(long, global = true)]
    no_emoji: bool,
    /// Table style: plain|markdown|box|csv
    #[arg(long, global = true, value_name = "STYLE")]
    table_style: Option<String>,
    /// Enable timestamps
    #[arg(long, global = true)]
    timestamps: bool,
    /// Disable timestamps (wins over --timestamps)
    #[arg(long, global = true)]
    no_timestamps: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a capability with telemetry
    #[command(
        about = "exec capability with telemetry",
        after_help = "CAPABILITY HELP:\n runtimo run -c <Cap> --schema     → see expected JSON args\n runtimo run -c <Cap> --dry-run    → validate without executing\n\nEXAMPLES:\n runtimo run -c FileRead -a '{\"path\":\"/tmp/test.txt\"}'\n runtimo run -c ShellExec -a '{\"cmd\":\"uptime\"}'\n runtimo run -c FileWrite -a '{\"path\":\"/tmp/x.txt\",\"content\":\"hello\"}'\n\nBlocked commands: rm, shutdown, chmod, mkfs, dd, iptables, fork bombs, env dumpers.\nSee `runtimo list` for the full blocklist.\n\nARGS SIZE:\n Maximum args payload is ~130 KB. For larger payloads, use --args-file <path> or --args-stdin."
    )]
    Run {
        /// Capability name (e.g., FileRead, ShellExec). Use `runtimo list` to see all.
        #[arg(short = 'c', long)]
        capability: String,
        /// Capability arguments as JSON (e.g., '{"path":"/tmp/test.txt"}'). Use --schema to see the expected shape.
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        /// Path to a file containing capability arguments as JSON (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_file: Option<PathBuf>,
        /// Read capability arguments as JSON from stdin (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_stdin: bool,
        /// Validate args and check blocklist but don't execute
        #[arg(long)]
        dry_run: bool,
        /// Output raw JSON instead of formatted text
        #[arg(short = 'j', long)]
        json: bool,
        /// Suppress output except errors (useful for scripting)
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Print the capability's JSON Schema and exit
        #[arg(long)]
        schema: bool,
        /// Execution timeout in seconds (no upper bound). Defaults to config value, then 30s.
        #[arg(long, value_parser = clap::value_parser!(u64))]
        timeout: Option<u64>,
    },
    /// Dispatch job to background daemon (returns immediately)
    #[command(
        about = "Dispatch job to background daemon (starts daemon automatically if needed)",
        after_help = "EXAMPLES:\n runtimo dispatch -c ShellExec -a '{\"cmd\":\"sleep 30\"}'\n runtimo dispatch -c FileWrite -a '{\"path\":\"/tmp/x.txt\",\"content\":\"bg\"}'\n runtimo dispatch -c GitExec -a '{\"operation\":\"status\",\"path\":\"/tmp/repo\"}'\n\nAfter dispatch:\n runtimo status                # check all job statuses\n runtimo wait -j <job_id>      # wait for completion\n runtimo logs -j <job_id>      # view WAL events\n\nDaemon starts automatically on first dispatch.\n\nARGS SIZE:\n Maximum args payload is ~130 KB. For larger payloads, use --args-file <path> or --args-stdin."
    )]
    Dispatch {
        /// Capability name (e.g., ShellExec, FileWrite). Use `runtimo list` to see all.
        #[arg(short = 'c', long)]
        capability: String,
        /// Capability arguments as JSON (same format as `run`)
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        /// Path to a file containing capability arguments as JSON (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_file: Option<PathBuf>,
        /// Read capability arguments as JSON from stdin (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_stdin: bool,
        /// Validate and check blocklist but don't enqueue the job
        #[arg(long)]
        dry_run: bool,
    },
    /// Wait for a dispatched job to complete
    ///
    /// Pre-validates job existence via daemon RPC or WAL scan before entering
    /// the poll loop. Returns immediately with "Job not found" if the job ID
    /// is unknown and the daemon is unreachable.
    #[command(
        about = "Wait for a dispatched job",
        after_help = "EXAMPLES:\n runtimo wait -j abc123\n runtimo wait -j abc123 --timeout 60"
    )]
    Wait {
        /// Job ID to wait for (from dispatch output or `runtimo jobs`)
        #[arg(short = 'j', long)]
        job_id: String,
        /// Maximum seconds to wait (0 = wait forever)
        #[arg(long, default_value = "0")]
        timeout: u64,
    },
    /// List available capabilities
    #[command(
        about = "List capabilities",
        after_help = "Use --schemas to see JSON argument schemas for each capability.\nUse --json for machine-readable output."
    )]
    List {
        /// Show each capability's JSON argument schema
        #[arg(long)]
        schemas: bool,
        /// Output as JSON (machine-readable)
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Check job status (via daemon RPC if running, falls back to WAL)
    #[command(
        about = "Check job status (daemon RPC or WAL fallback)",
        after_help = "EXAMPLES:\n runtimo status             # all jobs (daemon RPC)\n runtimo status -j abc123   # specific job\n runtimo status -oj         # JSON output\n\nNote: queries daemon for live status; falls back to WAL data if daemon unreachable."
    )]
    Status {
        /// Job ID to check (omit to list all)
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Output raw JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// List recent jobs from WAL (local + dispatched, read-only snapshot)
    #[command(
        about = "List recent jobs from WAL",
        after_help = "EXAMPLES:\n runtimo jobs\n runtimo jobs --limit 5\n runtimo jobs --json\n\nNote: reads from WAL directly (no daemon needed). Use `status` for live daemon query."
    )]
    Jobs {
        /// Number of jobs to show (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Output raw JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// View WAL logs (audit trail of all events)
    #[command(
        about = "View WAL logs",
        after_help = "EXAMPLES:\n runtimo logs              # last 10 events\n runtimo logs -j abc123    # events for a specific job\n runtimo logs -n 50        # last 50 events\n runtimo logs -oj          # JSON output"
    )]
    Logs {
        /// Filter by job ID
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Number of events to show (default: 10)
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Output raw JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// Undo a completed job (restore files from backup)
    #[command(
        about = "Undo a completed job",
        after_help = "Find job IDs with `runtimo jobs` or `runtimo logs`.\n\nEXAMPLES:\n runtimo undo -j abc123\n runtimo undo -j abc123 --dry-run    # check what would be restored"
    )]
    Undo {
        /// Job ID to undo (from `runtimo jobs` or `runtimo logs`)
        #[arg(short = 'j', long)]
        job_id: String,
        /// Show what files would be restored without actually restoring them
        #[arg(long)]
        dry_run: bool,
    },
    /// Print system telemetry (CPU, RAM, disk, GPU, network)
    #[command(
        about = "Print system telemetry",
        after_help = "EXAMPLES:\n runtimo telemetry             # formatted\n runtimo telemetry -j          # JSON\n runtimo telemetry -v          # include listening ports\n runtimo telemetry -jv         # JSON with verbose"
    )]
    Telemetry {
        /// Output raw JSON
        #[arg(short = 'j', long)]
        json: bool,
        /// Show extended details (listening ports, GPU info)
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Print process snapshot (top consumers, zombie count)
    #[command(
        about = "Print process snapshot",
        after_help = "EXAMPLES:\n runtimo processes             # formatted table\n runtimo processes -j          # JSON output"
    )]
    Processes {
        /// Output raw JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List and optionally reap zombie processes
    #[command(
        about = "List zombie processes",
        after_help = "EXAMPLES:\n runtimo zombies\n runtimo zombies --reap\n\nZombies are dead processes whose parents haven't called waitpid(2).\nThey can't be killed directly. --reap kills each zombie's parent process\ninstead, which causes the kernel to clean up the zombie."
    )]
    Zombies {
        #[arg(short = 'r', long, default_value = "false")]
        reap: bool,
    },
    #[command(
        about = "Manage configuration",
        after_help = "Config file: ~/.config/runtimo/config.toml\n\
Supported fields:\n\
  allowed_paths       Extra path prefixes for FileRead/FileWrite\n\
  dal                 Design Assurance Level A-E\n\
  blocklist_overrides Additional ShellExec blocklist patterns\n\
  capability_timeouts Per-capability timeout overrides\n\
\nExample:\n\
  allowed_paths = [\"/srv\", \"/opt\"]\n\
  dal = \"B\"\n\
  [capability_timeouts]\n\
  ShellExec = 120"
    )]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Session step-runner — deterministic bounded execution from a prompt file
    #[command(about = "Session step-runner (MVP)")]
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Run steps from a prompt file as a bounded session
    Run {
        /// Prompt file containing JSONL or markdown ```runtimo blocks
        #[arg(long, value_name = "PATH")]
        prompt_file: PathBuf,
        /// Session name (human-readable) — reuses existing session if name or id matches, else creates a new one
        #[arg(long)]
        session: Option<String>,
        /// Maximum number of steps to execute (default: from ResolvedConfig — 20 ephemeral, 100 service/minimal)
        #[arg(long)]
        max_steps: Option<u32>,
        /// Maximum total seconds for the session run (default: from ResolvedConfig — 300 ephemeral, 3600 service)
        #[arg(long)]
        max_seconds: Option<u64>,
        /// Behavior on step failure: continue|stop (default: from ResolvedConfig — continue ephemeral, stop service)
        #[arg(long, value_name = "POLICY")]
        on_failure: Option<String>,
        /// Validate prompt file and policy only; do not execute or create a session
        #[arg(long, default_value = "false")]
        dry_run: bool,
        /// Stub: dispatch each step via daemon RPC instead of local execution
        #[arg(long, default_value = "false")]
        via_daemon: bool,
    },
    /// List sessions persisted under the sessions directory
    List {
        /// Output raw JSON
        #[arg(long, default_value = "false")]
        json: bool,
    },
    /// Show a single session by id
    Show {
        /// Session ID to show
        #[arg(long, value_name = "SESSION_ID")]
        session_id: String,
        /// Output raw JSON
        #[arg(long, default_value = "false")]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    #[command(about = "Manage allowed path prefixes for FileRead/FileWrite")]
    AllowedPaths {
        #[command(subcommand)]
        subaction: AllowedPathsAction,
    },
    /// Show current configuration
    #[command(about = "Show current configuration (from config.toml)")]
    Show,
    /// Get or set the Design Assurance Level (DAL)
    #[command(
        about = "Get or set the Design Assurance Level (A-E) for the cognitive safety pipeline"
    )]
    Dal {
        /// New DAL level to set (A, B, C, D, or E). Omit to show current value.
        level: Option<String>,
    },
    /// Initialize config file from profile template
    #[command(about = "Initialize config file from profile template (minimal/ephemeral/service)")]
    Init {
        /// Profile to use (minimal, ephemeral, service). Use --minimal as alias for minimal.
        #[arg(long)]
        profile: Option<String>,
        /// Overwrite existing config
        #[arg(long, default_value = "false")]
        force: bool,
        /// Alias for --profile minimal
        #[arg(long, default_value = "false")]
        minimal: bool,
        /// Custom path for config file (default: XDG config path)
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AllowedPathsAction {
    Add { paths: Vec<String> },
    Remove { paths: Vec<String> },
    List,
}

/// Returns the WAL file path (env-overridable via `RUNTIMO_WAL_PATH`).
fn wal_path() -> PathBuf {
    runtimo_core::utils::wal_path()
}

/// Returns the backup directory derived from `data_dir()`.
///
/// Delegates to [`runtimo_core::utils::backup_dir`], which always
/// returns `data_dir().join("backups")` — no env var override (ADR-C28).
fn backup_dir() -> PathBuf {
    runtimo_core::utils::backup_dir()
}

/// Creates a capability registry with all built-in capabilities registered.
///
/// # Returns
///
/// `Ok(CapabilityRegistry)` — All capabilities registered successfully.
/// `Err(String)` — FileWrite or GitExec initialization failed (e.g. backup
/// directory cannot be created).
fn make_registry() -> Result<CapabilityRegistry, String> {
    let mut reg = CapabilityRegistry::new();
    reg.register(FileRead);
    reg.register(FileWrite::new().map_err(|e| format!("FileWrite init failed: {}", e))?);
    reg.register(Delete::new().map_err(|e| format!("Delete init failed: {}", e))?);
    reg.register(GitExec::new(backup_dir()).map_err(|e| format!("GitExec init failed: {}", e))?);
    reg.register(ShellExec);
    reg.register(Kill);
    reg.register(Undo);
    Ok(reg)
}

// Concurrency control for CLI run — mirrors daemon's MAX_CONCURRENT_JOBS = 16

/// Maximum concurrent CLI `run` invocations.
const MAX_CLI_CONCURRENT: usize = 16;
/// Global counter of currently-running CLI jobs.
static CLI_ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);

/// Returns "enabled"/"disabled" for config-show output.
fn on_off(v: bool) -> &'static str {
    if v {
        "enabled"
    } else {
        "disabled"
    }
}

/// Attempts to acquire a concurrency slot for a CLI `run` command.
///
/// Returns `false` if `MAX_CLI_CONCURRENT` slots are already in use.
fn acquire_cli_slot() -> bool {
    let current = CLI_ACTIVE_JOBS.fetch_add(1, Ordering::AcqRel);
    if current >= MAX_CLI_CONCURRENT {
        CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}

/// Releases a concurrency slot after a CLI `run` command completes.
fn release_cli_slot() {
    CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
}

// ── Daemon client ───────────────────────────────────────────────────────────

/// Returns the path to the daemon's Unix socket (`{data_dir}/runtimo.sock`).
fn daemon_socket() -> PathBuf {
    runtimo_core::utils::data_dir().join("runtimo.sock")
}

/// Finds the `runtimo-daemon` binary, first checking next to the CLI binary,
/// then falling back to `which` and `~/.cargo/bin/`.
fn find_daemon_binary() -> Option<PathBuf> {
    let cli_path = std::env::current_exe().ok()?;
    let dir = cli_path.parent()?;
    let daemon_path = dir.join("runtimo-daemon");
    if daemon_path.exists() {
        return Some(daemon_path);
    }
    dir.join(format!("runtimo-daemon{}", std::env::consts::EXE_SUFFIX))
        .exists()
        .then_some(daemon_path)
}

/// Searches `PATH` and `~/.cargo/bin/` for the `runtimo-daemon` binary.
fn find_daemon_in_path() -> Option<PathBuf> {
    let output = Command::new("which").arg("runtimo-daemon").output().ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    // Fallback: check standard cargo install directory
    let home = std::env::var("HOME").ok()?;
    let cargo_bin = PathBuf::from(home).join(".cargo/bin/runtimo-daemon");
    cargo_bin.exists().then_some(cargo_bin)
}

/// Returns the path to the daemon lock file (`{data_dir}/daemon.lock`).
fn daemon_lock_path() -> PathBuf {
    runtimo_core::utils::data_dir().join("daemon.lock")
}

/// Acquires an exclusive `flock` on the daemon lock file to prevent
/// race conditions when auto-starting the daemon from multiple processes.
///
/// Uses `LOCK_EX | LOCK_NB` — fails immediately if another process holds the lock.
///
/// # Errors
/// Returns an error string if the lock file cannot be created or the lock is held.
fn acquire_daemon_lock() -> Result<File, String> {
    use libc::flock;
    let lock_path = daemon_lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create lock dir: {}", e))?;
    }
    let file =
        File::create(&lock_path).map_err(|e| format!("Failed to create lock file: {}", e))?;
    // Try to acquire exclusive non-blocking lock using flock
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid file descriptor from File::create; LOCK_EX | LOCK_NB are valid flock flags
    let result = unsafe { flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err("Another process is starting the daemon".to_string());
    }
    Ok(file)
}

/// Checks whether the daemon is running by attempting to connect to its Unix socket.
fn daemon_is_running() -> bool {
    UnixStream::connect(daemon_socket()).is_ok()
}

/// Ensures the daemon is running, auto-starting it if necessary.
///
/// Uses a double-checked locking pattern with `acquire_daemon_lock` to prevent
/// multiple processes from spawning the daemon simultaneously. Waits up to
/// `DAEMON_STARTUP_TIMEOUT_SECS` (30s) for the daemon to become ready.
///
/// # Errors
/// Returns an error if the daemon binary cannot be found, the daemon fails to
/// start, or it doesn't become ready within the timeout.
fn ensure_daemon_running() -> Result<(), String> {
    if daemon_is_running() {
        return Ok(());
    }

    // Acquire lock before spawning daemon to prevent race condition
    let _lock = acquire_daemon_lock()?;

    // Double-check after acquiring lock
    if daemon_is_running() {
        return Ok(());
    }

    let daemon_bin = find_daemon_binary()
        .or_else(find_daemon_in_path)
        .ok_or_else(|| {
            "runtimo-daemon binary not found. Is runtimo-daemon installed?".to_string()
        })?;

    let mut child = Command::new(&daemon_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start daemon ({}): {}", daemon_bin.display(), e))?;

    #[allow(clippy::arithmetic_side_effects)]
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_secs(DAEMON_STARTUP_TIMEOUT_SECS))
        .unwrap_or_else(|| {
            std::time::Instant::now() + Duration::from_secs(DAEMON_STARTUP_TIMEOUT_SECS)
        });
    loop {
        if daemon_is_running() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let err_msg = if let Ok(Some(status)) = child.try_wait() {
                let mut stderr = String::new();
                if let Some(ref mut pipe) = child.stderr {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                if stderr.is_empty() {
                    format!(
                        "Daemon exited with status {} before becoming ready. No error output.",
                        status
                    )
                } else {
                    format!("Daemon exited with status {}: {}", status, stderr.trim())
                }
            } else {
                format!(
                    "Daemon started but did not become ready within {}s",
                    DAEMON_STARTUP_TIMEOUT_SECS
                )
            };
            let _ = child.kill();
            return Err(err_msg);
        }
        // Check if daemon exited early
        if let Ok(Some(status)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(ref mut pipe) = child.stderr {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let msg = if stderr.is_empty() {
                format!("Daemon exited early with status {}", status)
            } else {
                format!("Daemon exited early: {}", stderr.trim())
            };
            return Err(msg);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Resolves capability arguments from the appropriate source.
///
/// Priority: --args-file > --args-stdin > -a (default).
/// Validates that --args-file and --args-stdin are not used simultaneously.
/// Validates content size against MAX_ARGS_SIZE_BYTES (~130 KB).
fn resolve_args(
    args: &str,
    args_file: Option<PathBuf>,
    args_stdin: bool,
) -> Result<String, String> {
    if args_file.is_some() && args_stdin {
        return Err("Cannot use both --args-file and --args-stdin simultaneously".to_string());
    }

    let content = if let Some(file_path) = args_file {
        let mut content = String::new();
        File::open(&file_path)
            .map_err(|e| format!("Failed to open args file {}: {}", file_path.display(), e))?
            .read_to_string(&mut content)
            .map_err(|e| format!("Failed to read args file {}: {}", file_path.display(), e))?;
        content
    } else if args_stdin {
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(|e| format!("Failed to read args from stdin: {}", e))?;
        content
    } else {
        args.to_string()
    };

    if content.len() > MAX_ARGS_SIZE_BYTES {
        return Err(format!(
            "Capability args too large: {} bytes (max: {} bytes / ~130 KB). \
             Use --args-file or --args-stdin for large payloads.",
            content.len(),
            MAX_ARGS_SIZE_BYTES,
        ));
    }

    Ok(content)
}

/// Sends a JSON-RPC request to the daemon over its Unix socket.
///
/// Serializes `method` and `params` into a JSON-RPC request, writes it to the
/// socket, and reads a single-line JSON-RPC response.
///
/// # Errors
/// Returns an error string if the daemon cannot be reached, the request cannot
/// be serialized, or the daemon returns an error.
fn send_rpc(method: &str, params: Value) -> Result<Value, String> {
    let sock_path = daemon_socket();
    let mut stream = UnixStream::connect(&sock_path).map_err(|e| {
        format!(
            "Cannot connect to daemon at {}: {}. Is `runtimo-daemon` running?",
            sock_path.display(),
            e
        )
    })?;

    let request = serde_json::json!({
        "method": method,
        "params": params,
        "id": 1,
    });
    let req_str = serde_json::to_string(&request).map_err(|e| format!("JSON encode: {}", e))?;
    stream
        .write_all(req_str.as_bytes())
        .map_err(|e| format!("Write: {}", e))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("Write nl: {}", e))?;

    // Use buffered reader for line-based reading — handles responses of any size
    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Read: {}", e))?;
    if line.is_empty() {
        return Err("Daemon closed connection".into());
    }

    let resp: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("JSON parse: {}", e))?;

    if let Some(err) = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(err.to_string());
    }

    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// Sentinel WAL sequence for a dispatched step with no completion record.
///
/// Real WAL sequences start at 0, so `u64::MAX` is distinct from every real
/// `wal_seq` — it marks "unknown", never a forged success marker like 0.
const DISPATCH_UNKNOWN_WAL_SEQ: u64 = u64::MAX;

/// Emits a session-loop diagnostic respecting the output mode.
///
/// In JSON mode prints a single-line JSON object to stdout (keeps stdout
/// parseable, stderr clean); in quiet mode suppresses loop progress noise;
/// otherwise writes the text to stderr as before.
fn emit_session_note(mode: &crate::output::OutputMode, value: Value, text: &str) {
    if mode.is_json() {
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        );
    } else if !mode.is_quiet() {
        eprintln!("{}", text);
    }
}

/// Resolves the WAL sequence of a dispatched job via the daemon `logs` RPC.
///
/// Returns the `seq` of the terminal (`job_completed`/`job_failed`) event,
/// or [`DISPATCH_UNKNOWN_WAL_SEQ`] when no completion record exists yet.
fn fetch_job_wal_seq(job_id: &str) -> u64 {
    if let Ok(v) = send_rpc("logs", serde_json::json!({ "job_id": job_id, "limit": 50 })) {
        if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
            for ev in events {
                let is_terminal = ev
                    .get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == "job_completed" || t == "job_failed");
                if is_terminal {
                    if let Some(seq) = ev.get("seq").and_then(|s| s.as_u64()) {
                        return seq;
                    }
                }
            }
        }
    }
    DISPATCH_UNKNOWN_WAL_SEQ
}

/// Polls the daemon `status` RPC until a dispatched job reaches a terminal
/// state or the time budget expires.
///
/// # Inputs
///
/// `job_id` — daemon job ID from `dispatch`. `budget_secs` — max seconds
/// to poll (caller passes the session's remaining `--max-seconds` budget).
///
/// # Outputs
///
/// `(success, error, wal_seq)` from the daemon's real terminal state —
/// never a synthesized success. `wal_seq` comes from the `logs` RPC, or
/// [`DISPATCH_UNKNOWN_WAL_SEQ`] when no completion record exists.
fn poll_dispatched_step(job_id: &str, budget_secs: u64) -> (bool, Option<String>, u64) {
    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(budget_secs.max(1));
    while start.elapsed() < budget {
        match send_rpc("status", serde_json::json!({ "job_id": job_id })) {
            Ok(v) => match v.get("status").and_then(|s| s.as_str()) {
                Some("completed") => return (true, None, fetch_job_wal_seq(job_id)),
                Some("failed") => {
                    let err = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .map(str::to_string)
                        .or_else(|| Some("execution reported failure".to_string()));
                    return (false, err, fetch_job_wal_seq(job_id));
                }
                _ => std::thread::sleep(std::time::Duration::from_secs(2)),
            },
            Err(_) => break,
        }
    }
    // Budget expired or daemon unreachable: check the daemon WAL via logs RPC
    // once before giving up (job may have completed between polls).
    let wal_seq = fetch_job_wal_seq(job_id);
    if wal_seq != DISPATCH_UNKNOWN_WAL_SEQ {
        if let Ok(v) = send_rpc("status", serde_json::json!({ "job_id": job_id })) {
            match v.get("status").and_then(|s| s.as_str()) {
                Some("completed") => return (true, None, wal_seq),
                Some("failed") => {
                    let err = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .map(str::to_string)
                        .or_else(|| Some("execution reported failure".to_string()));
                    return (false, err, wal_seq);
                }
                _ => {}
            }
        }
    }
    (
        false,
        Some("dispatch poll timed out before terminal status".to_string()),
        wal_seq,
    )
}

// ── Main ────────────────────────────────────────────────────────────────────

#[allow(
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::redundant_else,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]
fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let resolved = RuntimoConfig::load().resolved();
    let base_mode = OutputMode::from_cli(
        &resolved,
        cli.output.as_deref(),
        cli.color,
        cli.no_color,
        cli.emoji,
        cli.no_emoji,
        cli.table_style.as_deref(),
        cli.timestamps,
        cli.no_timestamps,
    );

    match cli.command {
        Commands::Run {
            capability,
            args,
            args_file,
            args_stdin,
            dry_run,
            json,
            quiet,
            schema,
            timeout,
        } => {
            let args = resolve_args(&args, args_file, args_stdin)?;
            let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
            if schema {
                if let Some(cap) = reg.get(&capability) {
                    println!("{}", cap.schema());
                } else {
                    eprintln!("Capability not found: {}. Use `runtimo list` to see available capabilities.", capability);
                    std::process::exit(1);
                }
                return Ok(());
            }
            let cap = reg.get(&capability).ok_or_else(|| {
                format!(
                    "Capability not found: {}. Use `runtimo list` to see available capabilities.",
                    capability
                )
            })?;
            let args_val: Value =
                serde_json::from_str(&args).map_err(|e| format!("Invalid JSON args: {}", e))?;
            if let Err(e) = cap.validate(&args_val) {
                eprintln!("Validation failed: {}", e);
                std::process::exit(1);
            }
            // Acquire concurrency slot (mirrors daemon's MAX_CONCURRENT_JOBS)
            if !acquire_cli_slot() {
                eprintln!(
                    "Too many concurrent CLI runs (max {}). Try again later.",
                    MAX_CLI_CONCURRENT
                );
                std::process::exit(1);
            }
            let resolved_timeout =
                timeout.unwrap_or_else(|| RuntimoConfig::get_capability_timeout(&capability, 30));
            let result = execute_with_telemetry_and_session(
                cap,
                &args_val,
                dry_run,
                &wal_path(),
                None,
                None,
                resolved_timeout,
            )
            .map_err(|e| format!("{}", e));
            release_cli_slot();
            let result = result?;
            if !result.success {
                // Failure must respect the output mode: JSON stays parseable
                // on stdout, quiet stays silent (exit code carries the signal).
                let fail_is_json = json || base_mode.is_json();
                let fail_is_quiet = !fail_is_json && (quiet || base_mode.is_quiet());
                if fail_is_json {
                    println!(
                        "{}",
                        serde_json::json!({"success": false, "capability": capability, "output": result.output})
                    );
                } else if !fail_is_quiet {
                    eprintln!("{}", result.output.output);
                }
                std::process::exit(1);
            }
            let output = result.output;
            // Resolve effective mode: subcommand --json/--quiet override global --output
            let mode = if json {
                base_mode.with_format("json")
            } else if quiet {
                base_mode.with_format("quiet")
            } else {
                base_mode
            };
            if mode.is_json() {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if mode.is_quiet() {
                // silent — preserve --quiet contract
            } else {
                println!("{}", mode.render_text(&output.output));
                if let Some(ref data) = output.data {
                    let text = if let Some(s) = data.as_str() {
                        s.to_string()
                    } else {
                        data.to_string()
                    };
                    let rendered = mode.render_text(&text);
                    if !rendered.trim().is_empty() {
                        println!("{}", rendered);
                    }
                }
            }
        }

        Commands::Dispatch {
            capability,
            args,
            args_file,
            args_stdin,
            dry_run,
        } => {
            if let Err(e) = ensure_daemon_running() {
                eprintln!("Cannot dispatch: {}", e);
                std::process::exit(1);
            }
            let args = resolve_args(&args, args_file, args_stdin)?;
            let args_val: Value =
                serde_json::from_str(&args).map_err(|e| format!("Invalid JSON args: {}", e))?;
            // Pre-validate dangerous commands at dispatch time
            if capability == "ShellExec" {
                if let Some(cmd) = args_val.get("cmd").and_then(|v| v.as_str()) {
                    if let Some(reason) = is_dangerous_command(cmd) {
                        eprintln!("Dispatch rejected: dangerous command blocked: {}", reason);
                        std::process::exit(1);
                    }
                    if !runtimo_core::capabilities::network_enabled()
                        && runtimo_core::capabilities::is_network_command(cmd)
                    {
                        eprintln!("Dispatch rejected: network commands blocked — set RUNTIMO_ENABLE_NETWORK=1 to enable");
                        std::process::exit(1);
                    }
                }
            }
            let params = serde_json::json!({
                "capability": capability,
                "args": args_val,
                "dry_run": dry_run,
                "working_dir": std::env::current_dir().unwrap_or_default().to_string_lossy(),
            });
            match send_rpc("dispatch", params) {
                Ok(result) => {
                    let jid = result.get("job_id").and_then(|v| v.as_str()).unwrap_or("?");
                    let cap = result
                        .get("capability")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    println!("Dispatched job {} (capability: {})", jid, cap);
                    println!("Check status: runtimo wait -j {}", jid);
                }
                Err(e) => {
                    let mut msg = format!("Dispatch failed: {}", e);
                    if e.contains("Capability not found") {
                        msg.push_str("\nUse `runtimo list` to see available capabilities.");
                    }
                    eprintln!("{}", msg);
                    std::process::exit(1);
                }
            }
        }

        Commands::Wait { job_id, timeout } => {
            // Early validation: reject empty job_id
            if job_id.is_empty() {
                eprintln!("Job ID cannot be empty");
                std::process::exit(1);
            }
            // Pre-validate: check if job exists before entering poll loop
            // Try daemon RPC first
            let job_exists = send_rpc("status", serde_json::json!({ "job_id": &job_id }))
                .map_or_else(
                    |_| {
                        // Daemon unreachable; check WAL for any job event
                        if let Ok(reader) = WalReader::load_all(&wal_path()) {
                            reader.events().iter().any(|e| {
                                e.job_id == job_id
                                    && matches!(
                                        e.event_type,
                                        runtimo_core::WalEventType::JobStarted
                                            | runtimo_core::WalEventType::JobCompleted
                                            | runtimo_core::WalEventType::JobFailed
                                    )
                            })
                        } else {
                            false
                        }
                    },
                    |result| {
                        let status = result
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        status != "unknown"
                    },
                );

            if !job_exists {
                eprintln!("Job not found: {}", job_id);
                std::process::exit(1);
            }

            let start = std::time::Instant::now();
            loop {
                let params = serde_json::json!({ "job_id": &job_id });
                #[allow(clippy::single_match_else)]
                // refactoring to if-let-else changes control flow here
                match send_rpc("status", params) {
                    Ok(result) => {
                        let status = result
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        match status {
                            "running" => {
                                if timeout > 0 && start.elapsed().as_secs() >= timeout {
                                    println!(
                                        "Job {} still running (timeout after {}s)",
                                        job_id, timeout
                                    );
                                    return Ok(());
                                }
                                let elapsed = start.elapsed().as_secs();
                                if elapsed > 0 && elapsed.is_multiple_of(10) {
                                    println!(
                                        "Job {} still running ({}s elapsed)...",
                                        job_id, elapsed
                                    );
                                }
                                std::thread::sleep(std::time::Duration::from_secs(2));
                            }
                            "completed" => {
                                println!("Job {} completed", job_id);
                                return Ok(());
                            }
                            "failed" => {
                                println!("Job {} failed", job_id);
                                return Ok(());
                            }
                            _ => {
                                println!("Job {} status: {}", job_id, status);
                                return Ok(());
                            }
                        }
                    }
                    Err(_) => {
                        // Daemon might not be running; check WAL directly
                        if let Ok(reader) = WalReader::load_all(&wal_path()) {
                            let events = reader.events();
                            let has_completed = events.iter().any(|e| {
                                e.job_id == job_id
                                    && matches!(
                                        e.event_type,
                                        runtimo_core::WalEventType::JobCompleted
                                    )
                            });
                            if has_completed {
                                println!("Job {} completed (checked via WAL)", job_id);
                                return Ok(());
                            }
                            let has_failed = events.iter().any(|e| {
                                e.job_id == job_id
                                    && matches!(e.event_type, runtimo_core::WalEventType::JobFailed)
                            });
                            if has_failed {
                                println!("Job {} failed (checked via WAL)", job_id);
                                return Ok(());
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
                if timeout > 0 && start.elapsed().as_secs() >= timeout {
                    println!("Job {} still pending (timeout after {}s)", job_id, timeout);
                    return Ok(());
                }
            }
        }

        Commands::List { schemas, json } => {
            let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
            // Effective mode: global --output json or local --json forces json
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            if mode.is_json() {
                let caps: Vec<Value> = reg
                    .list()
                    .iter()
                    .filter_map(|name| {
                        reg.get(name).map(|cap| {
                        serde_json::json!({
                            "name": name,
                            "description": cap.description(),
                            "schema": if schemas { Some(cap.schema().to_string()) } else { None },
                        })
                    })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&caps)?);
            } else if mode.is_quiet() {
                // silent
            } else {
                for name in reg.list() {
                    if let Some(cap) = reg.get(name) {
                        print!("  {:>12}  {}", name, cap.description());
                        if schemas {
                            println!("\n    schema: {}", cap.schema());
                        } else {
                            println!();
                        }
                    }
                }
            }
        }

        Commands::Status { job_id, json } => {
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            if let Some(jid) = job_id {
                // Try daemon RPC first
                if let Ok(result) = send_rpc("status", serde_json::json!({ "job_id": &jid })) {
                    if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else if mode.is_quiet() {
                        // silent
                    } else {
                        let headers = ["JOB_ID", "STATUS", "CAPABILITY"];
                        let rows = vec![vec![
                            result
                                .get("job_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                            result
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                            result
                                .get("capability")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                        ]];
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                    return Ok(());
                }

                // Fallback to WAL
                if let Ok(reader) = WalReader::load_all(&wal_path()) {
                    let events = reader.events();
                    let by_job: Vec<_> = events.iter().filter(|e| e.job_id == jid).collect();
                    if by_job.is_empty() {
                        println!("Job not found: {}", jid);
                    } else if mode.is_quiet() {
                        // silent
                    } else if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&by_job)?);
                    } else {
                        let headers = if mode.timestamps {
                            vec!["EVENT", "CAPABILITY", "TS"]
                        } else {
                            vec!["EVENT", "CAPABILITY"]
                        };
                        let rows: Vec<Vec<String>> = by_job
                            .iter()
                            .map(|e| {
                                if mode.timestamps {
                                    vec![
                                        e.event_type.as_str().to_string(),
                                        e.capability.as_deref().unwrap_or("-").to_string(),
                                        e.ts.to_string(),
                                    ]
                                } else {
                                    vec![
                                        e.event_type.as_str().to_string(),
                                        e.capability.as_deref().unwrap_or("-").to_string(),
                                    ]
                                }
                            })
                            .collect();
                        let hdr_refs: Vec<&str> = headers.clone();
                        println!("{}", mode.render_table(&hdr_refs, &rows));
                    }
                } else {
                    println!("Cannot read WAL");
                }
            } else {
                // List all jobs via daemon
                if let Ok(result) = send_rpc("jobs", serde_json::json!({ "limit": 50 })) {
                    if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else if mode.is_quiet() {
                        // silent
                    } else {
                        let jobs = result["jobs"].as_array().cloned().unwrap_or_default();
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else {
                            let headers = ["JOB_ID", "STATUS", "CAPABILITY"];
                            let rows: Vec<Vec<String>> = jobs
                                .iter()
                                .map(|job| {
                                    let status = job["status"].as_str().unwrap_or("?");
                                    let icon = mode.status_icon(status);
                                    vec![
                                        job["job_id"].as_str().unwrap_or("?").to_string(),
                                        format!("{}{}", icon, status),
                                        job["capability"].as_str().unwrap_or("?").to_string(),
                                    ]
                                })
                                .collect();
                            println!("{}", mode.render_table(&headers, &rows));
                        }
                    }
                } else {
                    // Fallback to WAL
                    if let Ok(reader) = WalReader::load_all(&wal_path()) {
                        let events = reader.events();
                        let mut seen: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        let mut rows: Vec<Vec<String>> = Vec::new();
                        for e in events.iter().rev() {
                            if seen.contains(&e.job_id) {
                                continue;
                            }
                            seen.insert(&e.job_id);
                            rows.push(vec![
                                e.job_id.clone(),
                                e.event_type.as_str().to_string(),
                                e.capability.as_deref().unwrap_or("-").to_string(),
                            ]);
                            if rows.len() >= 50 {
                                break;
                            }
                        }
                        if rows.is_empty() {
                            println!("No jobs found.");
                        } else if mode.is_quiet() {
                            // silent
                        } else if mode.is_json() {
                            println!("{}", serde_json::to_string_pretty(&rows)?);
                        } else {
                            let headers = ["JOB_ID", "EVENT", "CAPABILITY"];
                            if mode.timestamps {
                                // include ts if timestamps enabled
                                let rows_ts: Vec<Vec<String>> = rows
                                    .iter()
                                    .map(|r| {
                                        // find ts for this job id
                                        let ts = events
                                            .iter()
                                            .find(|ev| ev.job_id == r[0])
                                            .map(|ev| ev.ts.to_string())
                                            .unwrap_or_default();
                                        vec![r[0].clone(), r[1].clone(), r[2].clone(), ts]
                                    })
                                    .collect();
                                let hdr = ["JOB_ID", "EVENT", "CAPABILITY", "TS"];
                                println!("{}", mode.render_table(&hdr, &rows_ts));
                            } else {
                                println!("{}", mode.render_table(&headers, &rows));
                            }
                        }
                    }
                }
            }
        }

        Commands::Jobs { limit, json } => {
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            // Try daemon RPC first
            let result = send_rpc("jobs", serde_json::json!({ "limit": limit }));
            match result {
                Ok(data) => {
                    if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&data)?);
                    } else if mode.is_quiet() {
                        // silent
                    } else {
                        let jobs = data["jobs"].as_array().cloned().unwrap_or_default();
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else {
                            let headers = ["JOB_ID", "CAPABILITY", "STATUS"];
                            let rows: Vec<Vec<String>> = jobs
                                .iter()
                                .map(|j| {
                                    let status = j["status"].as_str().unwrap_or("?");
                                    let icon = mode.status_icon(status);
                                    vec![
                                        j["job_id"].as_str().unwrap_or("?").to_string(),
                                        j["capability"].as_str().unwrap_or("?").to_string(),
                                        format!("{}{}", icon, status),
                                    ]
                                })
                                .collect();
                            println!("{}", mode.render_table(&headers, &rows));
                        }
                    }
                }
                Err(_) => {
                    // Fallback to WAL
                    if let Ok(reader) = WalReader::load_all(&wal_path()) {
                        let events = reader.events();
                        let mut jobs: Vec<Value> = Vec::new();
                        let mut seen: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        for e in events.iter().rev() {
                            if seen.contains(&e.job_id) {
                                continue;
                            }
                            if jobs.len() >= limit {
                                break;
                            }
                            seen.insert(&e.job_id);
                            let status = match e.event_type {
                                runtimo_core::WalEventType::JobStarted => "started",
                                runtimo_core::WalEventType::JobCompleted => "completed",
                                runtimo_core::WalEventType::JobFailed => "failed",
                                _ => "unknown",
                            };
                            jobs.push(serde_json::json!({
                                "job_id": e.job_id,
                                "capability": e.capability,
                                "status": status,
                                "started_at": e.ts,
                            }));
                        }
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else if mode.is_json() {
                            println!("{}", serde_json::to_string_pretty(&jobs)?);
                        } else if mode.is_quiet() {
                            // silent
                        } else {
                            let headers = ["JOB_ID", "CAPABILITY", "STATUS"];
                            let rows: Vec<Vec<String>> = jobs
                                .iter()
                                .map(|j| {
                                    let status = j["status"].as_str().unwrap_or("?");
                                    let icon = mode.status_icon(status);
                                    vec![
                                        j["job_id"].as_str().unwrap_or("?").to_string(),
                                        j["capability"].as_str().unwrap_or("?").to_string(),
                                        format!("{}{}", icon, status),
                                    ]
                                })
                                .collect();
                            println!("{}", mode.render_table(&headers, &rows));
                        }
                    } else {
                        eprintln!("Cannot read WAL. Is the daemon running?");
                    }
                }
            }
        }

        Commands::Logs {
            job_id,
            limit,
            json,
        } => {
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            // Try daemon RPC first
            let mut params = serde_json::json!({ "limit": limit });
            if let Some(ref jid) = job_id {
                params["job_id"] = serde_json::json!(jid);
            }
            if let Ok(result) = send_rpc("logs", params) {
                if mode.is_json() {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else if mode.is_quiet() {
                    // silent
                } else {
                    let events = result["events"].as_array().cloned().unwrap_or_default();
                    if events.is_empty() {
                        println!("No events found.");
                    } else {
                        let headers: Vec<&str> = if mode.timestamps {
                            vec!["TS", "JOB_ID", "EVENT", "CAPABILITY"]
                        } else {
                            vec!["JOB_ID", "EVENT", "CAPABILITY"]
                        };
                        let rows: Vec<Vec<String>> = events
                            .iter()
                            .map(|e| {
                                let ts = e["ts"].as_u64().unwrap_or(0).to_string();
                                let et = e["event_type"].as_str().unwrap_or("?").to_string();
                                let jid = e["job_id"].as_str().unwrap_or("?").to_string();
                                let cap = e["capability"].as_str().unwrap_or("-").to_string();
                                if mode.timestamps {
                                    vec![ts, jid, et, cap]
                                } else {
                                    vec![jid, et, cap]
                                }
                            })
                            .collect();
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
            } else if let Ok(reader) = WalReader::load_all(&wal_path()) {
                let events = reader.events();
                let filtered: Vec<_> = if let Some(ref jid) = job_id {
                    events.iter().filter(|e| e.job_id == *jid).collect()
                } else {
                    events.iter().collect()
                };
                let recent: Vec<_> = filtered.iter().rev().take(limit).rev().collect();
                if mode.is_json() {
                    println!("{}", serde_json::to_string_pretty(&recent)?);
                } else if mode.is_quiet() {
                    // silent
                } else if recent.is_empty() {
                    println!("No events found.");
                } else {
                    let headers: Vec<&str> = if mode.timestamps {
                        vec!["TS", "JOB_ID", "EVENT", "CAPABILITY"]
                    } else {
                        vec!["JOB_ID", "EVENT", "CAPABILITY"]
                    };
                    let rows: Vec<Vec<String>> = recent
                        .iter()
                        .map(|e| {
                            if mode.timestamps {
                                vec![
                                    e.ts.to_string(),
                                    e.job_id.clone(),
                                    e.event_type.as_str().to_string(),
                                    e.capability.as_deref().unwrap_or("-").to_string(),
                                ]
                            } else {
                                vec![
                                    e.job_id.clone(),
                                    e.event_type.as_str().to_string(),
                                    e.capability.as_deref().unwrap_or("-").to_string(),
                                ]
                            }
                        })
                        .collect();
                    println!("{}", mode.render_table(&headers, &rows));
                }
            }
        }

        Commands::Undo { job_id, dry_run } => {
            let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
            let cap = reg.get("Undo").ok_or("Undo capability not available")?;
            let args = serde_json::json!({ "job_id": job_id });
            let ctx = runtimo_core::Context {
                dry_run,
                job_id: runtimo_core::utils::generate_id(),
                working_dir: std::env::current_dir().unwrap_or_default(),
            };
            let output = cap.execute(&args, &ctx).map_err(|e| format!("{}", e))?;
            println!("{}", output.output);
        }

        Commands::Telemetry { json, verbose } => {
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            // Telemetry gate (RC1): when disabled, skip capture entirely —
            // no /proc reads, no subprocess probes, no display emitters.
            if !resolved.telemetry_enabled {
                if mode.is_json() {
                    println!(
                        "{}",
                        serde_json::json!({"telemetry_enabled": false, "telemetry": null})
                    );
                } else if !mode.is_quiet() {
                    println!(
                        "{}",
                        mode.render_text("Telemetry disabled (telemetry.enabled = false).")
                    );
                }
                return Ok(());
            }
            let tel = Telemetry::capture();
            if mode.is_json() {
                println!("{}", serde_json::to_string_pretty(&tel)?);
            } else if mode.is_quiet() {
                // silent
            } else {
                // Listening ports: shown only with --verbose flag
                let ports_str = if verbose && !tel.network.listening_ports.is_empty() {
                    format!(
                        "\nListening ports: {}",
                        tel.network
                            .listening_ports
                            .iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else {
                    String::new()
                };
                let text = format!(
                    "RUNTIMO TELEMETRY\n\nSystem\nCPU: {} ({} cores)\nRAM: {} total, {} free, {} available\nDisk: {} total, {} free ({}% used)\nUptime: {} ({}s)\nLoad: {} ({} cores)\n\nHardware\nAccelerators: {}\n\nNetwork\nPublic IP: {}\nTunnel: {}{}",
                    tel.system.cpu_model, tel.system.cpu_count,
                    tel.system.ram_total, tel.system.ram_free, tel.system.ram_available,
                    tel.system.disk_total, tel.system.disk_free, tel.system.disk_used_percent,
                    tel.system.uptime, tel.system.uptime_seconds,
                    tel.system.load_average, tel.system.cpu_count,
                    if tel.hardware.accelerators.is_empty() { "none".into() } else {
                        tel.hardware.accelerators.iter().map(|a| format!("{}: {}x", a.kind, a.count)).collect::<Vec<_>>().join(", ")
                    },
                    tel.network.public_ip,
                    if tel.network.tunnel_running {
                        format!("cloudflared (PID {})", tel.network.tunnel_pid.map_or_else(|| "?".to_string(), |p| p.to_string()))
                    } else {
                        "none".to_string()
                    },
                    ports_str,
                );
                println!("{}", mode.render_text(&text));
            }
        }

        Commands::Processes { json } => {
            let mode = if json {
                base_mode.with_format("json")
            } else {
                base_mode
            };
            let snap = ProcessSnapshot::capture();
            if mode.is_json() {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else if mode.is_quiet() {
                // silent
            } else {
                let zombie_lines = {
                    let zs = snap.zombies();
                    if zs.is_empty() {
                        String::new()
                    } else {
                        let lines: Vec<String> = zs
                            .iter()
                            .map(|p| {
                                format!(
                                    "- {} PPID:{} {} {}",
                                    p.pid,
                                    p.ppid,
                                    p.stat,
                                    p.command.chars().take(40).collect::<String>()
                                )
                            })
                            .collect();
                        format!("\n\nZombies ({})\n{}", zs.len(), lines.join("\n"))
                    }
                };
                let text = format!(
                    "PROCESS SNAPSHOT\n\nSummary\nTotal: {}\nCPU: {:.1}%\nMemory: {:.1}%\nZombies: {}{}\n\nTop CPU\n{}\n\nTop Memory\n{}",
                    snap.summary.total_processes,
                    snap.summary.total_cpu_percent,
                    snap.summary.total_mem_percent,
                    snap.summary.zombie_count,
                    zombie_lines,
                    snap.top_by_cpu(5).iter().map(|p| format!("- {} {} {} {}% CPU", p.pid, p.command.chars().take(40).collect::<String>(), p.stat, p.cpu_percent)).collect::<Vec<_>>().join("\n"),
                    snap.top_by_mem(5).iter().map(|p| format!("- {} {} {} {}% MEM", p.pid, p.command.chars().take(40).collect::<String>(), p.stat, p.mem_percent)).collect::<Vec<_>>().join("\n"),
                );
                println!("{}", mode.render_text(&text));
            }
        }

        Commands::Zombies { reap } => {
            let snap = ProcessSnapshot::capture();
            let zombies = snap.zombies();
            if zombies.is_empty() {
                println!("No zombie processes.");
                return Ok(());
            }

            println!("{} zombie(s) found:\n", zombies.len());
            for z in &zombies {
                println!(
                    "  {:>8}  PPID:{:>8}  {:>6}  {}",
                    z.pid, z.ppid, z.stat, z.command
                );
            }

            if reap {
                // Zombies can't be killed — they're already dead. We kill their
                // parent instead, which causes the kernel to reap the zombie.
                // Kill capability protects init (PID 1) and self.
                let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
                let killer = reg.get("Kill").ok_or("Kill capability not available")?;
                let mut unique_parents: std::collections::HashSet<u32> = zombies
                    .iter()
                    .map(|z| z.ppid)
                    .filter(|&ppid| ppid > 1)
                    .collect();
                // Never kill our own parent
                unique_parents.remove(&std::process::id());

                if unique_parents.is_empty() {
                    println!("\nNo reapable parents (all zombies are children of init or self).");
                    return Ok(());
                }

                println!("\nReaping via {} parent(s):", unique_parents.len());
                for ppid in &unique_parents {
                    print!("  PID {} → ", ppid);
                    let ctx = runtimo_core::Context {
                        dry_run: false,
                        job_id: format!("reap-{}", ppid),
                        working_dir: std::env::current_dir().unwrap_or_default(),
                    };
                    match killer.execute(&serde_json::json!({"pid": ppid, "signal": 15}), &ctx) {
                        Ok(o) => println!("{}", o.output),
                        Err(e) => println!("blocked: {}", e),
                    }
                }
                // Re-check
                ProcessSnapshot::clear_cache();
                let after = ProcessSnapshot::capture();
                let remaining = after.zombies().len();
                if remaining == 0 {
                    println!("\nAll zombies reaped.");
                } else {
                    println!(
                        "\n{} zombie(s) remain (may need SIGKILL or parent is protected).",
                        remaining
                    );
                }
            } else {
                println!(
                    "\nUse `runtimo zombies --reap` to kill zombie parents and clean them up."
                );
            }
        }

        Commands::Config { action } => match action {
            ConfigAction::AllowedPaths { subaction } => {
                let mut config = RuntimoConfig::load();
                match subaction {
                    AllowedPathsAction::Add { paths } => {
                        for p in paths {
                            if !config.allowed_paths.contains(&p) {
                                config.allowed_paths.push(p);
                            }
                        }
                        config.save().map_err(|e| format!("Save failed: {}", e))?;
                        println!("Prefixes updated: {:?}", config.allowed_paths);
                    }
                    AllowedPathsAction::Remove { paths } => {
                        config.allowed_paths.retain(|p| !paths.contains(p));
                        config.save().map_err(|e| format!("Save failed: {}", e))?;
                        println!("Prefixes updated: {:?}", config.allowed_paths);
                    }
                    AllowedPathsAction::List => {
                        let all = RuntimoConfig::get_allowed_prefixes();
                        println!("Allowed path prefixes:");
                        for p in all {
                            println!("  {}", p);
                        }
                    }
                }
            }
            ConfigAction::Show => {
                // Respect global --output json/quiet
                if base_mode.is_quiet() {
                    return Ok(());
                }
                if base_mode.is_json() {
                    let config_path = RuntimoConfig::config_path();
                    let config = RuntimoConfig::load();
                    let resolved = config.resolved();
                    let out = serde_json::json!({
                        "config_path": config_path.display().to_string(),
                        "profile": resolved.profile,
                        "dal": resolved.dal,
                        "wal_mode": resolved.wal_mode,
                        "backup_enabled": resolved.backup_enabled,
                        "output_format": resolved.output_format,
                        "output_renderer": resolved.output_renderer,
                        "blocklist_enabled": resolved.blocklist_enabled,
                        "session_max": resolved.session_max,
                        "session_timeout": resolved.session_timeout,
                        "session_on_limit": resolved.session_on_limit,
                        "telemetry_enabled": resolved.telemetry_enabled,
                    });
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    return Ok(());
                }
                let config_path = RuntimoConfig::config_path();
                let config_exists = config_path.exists();
                let config = match RuntimoConfig::load_result() {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!();
                        eprintln!("[runtimo] Config parse error: {}", e);
                        eprintln!(
                            "[runtimo] Showing defaults — fix the config file to apply settings."
                        );
                        eprintln!();
                        RuntimoConfig::default()
                    }
                };
                println!("Configuration: {}", config_path.display());
                if !config_exists {
                    println!("  (file does not exist — using defaults)");
                }
                // Profile line (Gate 1 seam)
                let profile_name = config
                    .profile
                    .clone()
                    .unwrap_or_else(|| "minimal".to_string());
                println!("Profile: {}", profile_name);
                println!();
                println!("Allowed paths (config file): {:?}", config.allowed_paths);
                println!("DAL (config): {:?}", config.dal);
                println!("Blocklist overrides: {:?}", config.blocklist_overrides);
                if config.capability_timeouts.is_empty() {
                    println!("Capability timeouts: (none configured, using defaults)");
                } else {
                    println!("Capability timeouts:");
                    for (cap, timeout) in &config.capability_timeouts {
                        println!("  {}: {}s", cap, timeout);
                    }
                }
                if config.env.is_empty() {
                    println!("Env [env]: (none configured)");
                } else {
                    println!("Env [env]:");
                    for (key, value) in &config.env {
                        println!("  {} = {}", key, value);
                    }
                }
                // Show profile-derived tables if present
                if config.profile.is_some() {
                    println!(
                        "Output: format={:?} renderer={:?}",
                        config.output.format, config.output.renderer
                    );
                    println!(
                        "WAL: mode={:?} enabled={:?}",
                        config.wal.mode, config.wal.enabled
                    );
                    println!("Backup: enabled={:?}", config.backup.enabled);
                    println!(
                        "Guards: dal={:?} blocklist_enabled={:?}",
                        config.guards.dal, config.guards.blocklist_enabled
                    );
                    println!(
                        "Session: max={:?} timeout={:?} on_limit={:?}",
                        config.session.max_sessions,
                        config.session.timeout_secs,
                        config.session.on_limit
                    );
                    println!("Telemetry: enabled={:?}", config.telemetry.enabled);
                }
                println!();
                println!("Effective settings (with env var + defaults):");
                let resolved = config.resolved();
                println!("  Profile: {}", resolved.profile);
                println!("  DAL: {}", resolved.dal);
                println!("  WAL mode: {}", resolved.wal_mode);
                println!("  Backup: {}", on_off(resolved.backup_enabled));
                println!(
                    "  Output: {}/{}",
                    resolved.output_format, resolved.output_renderer
                );
                println!(
                    "  Session: {}/{}s {}",
                    resolved.session_max, resolved.session_timeout, resolved.session_on_limit
                );
                println!("  DAL (legacy get_dal): {}", RuntimoConfig::get_dal());
                println!(
                    "  ShellExec blocklist: {}",
                    on_off(resolved.blocklist_enabled)
                );
                println!(
                    "  Critical-files denylist: {}",
                    on_off(RuntimoConfig::critical_files_enabled())
                );
                println!(
                    "  Path whitelist: {}",
                    on_off(RuntimoConfig::path_restriction_enabled())
                );
                println!(
                    "  PATH sanitization: {}",
                    on_off(RuntimoConfig::path_sanitization_enabled())
                );
                let all_prefixes = RuntimoConfig::get_allowed_prefixes();
                println!("  Allowed paths ({} total):", all_prefixes.len());
                for p in &all_prefixes {
                    println!("    {}", p);
                }
                // Guards off warning (Gate 1)
                if RuntimoConfig::guards_off_via_profile(&resolved)
                    && resolved.profile == "ephemeral"
                {
                    eprintln!(
                        "[runtimo] WARNING: guards off via profile {} — not for service machines",
                        resolved.profile
                    );
                }
                if !config_exists {
                    println!();
                    println!("No config file found. To customize, create one at:");
                    println!("  {}", config_path.display());
                    println!();
                    println!("Supported fields:");
                    println!("  allowed_paths     — Extra path prefixes for FileRead/FileWrite (list of strings)");
                    println!("  dal               — Design Assurance Level A-E (string)");
                    println!("  blocklist_overrides — Additional dangerous command patterns (list of strings)");
                    println!("  capability_timeouts — Per-capability timeout overrides (table of string->number)");
                    println!("  env               — Environment vars for ShellExec children + runtime gates (table)");
                    println!("  blocklist_enabled — Disable ShellExec dangerous-command blocklist (bool, default true)");
                    println!("  critical_files_enabled — Disable critical-files denylist in FileWrite/Delete (bool, default true)");
                    println!("  path_restriction_enabled — Disable allowed-prefix path whitelist (bool, default true)");
                    println!("  path_sanitization_enabled — Disable forced PATH on ShellExec children (bool, default true)");
                    println!("  profile           — Profile name: minimal|ephemeral|service");
                    println!("  [output]          — format, renderer");
                    println!("  [wal]             — mode, enabled");
                    println!("  [backup]          — enabled");
                    println!("  [guards]          — dal, blocklist_enabled, etc.");
                    println!("  [session]         — max_sessions, timeout_secs, on_limit");
                    println!("  [telemetry]       — enabled");
                    println!();
                    println!("Example:");
                    println!("  allowed_paths = [\"/srv\", \"/opt\"]");
                    println!("  dal = \"B\"");
                    println!("  blocklist_overrides = [\"curl\", \"wget\"]");
                    println!("  profile = \"ephemeral\"");
                    println!();
                    println!("  [capability_timeouts]");
                    println!("  ShellExec = 120");
                    println!("  FileRead = 10");
                    println!();
                    println!("  [env]");
                    println!("  RUNTIMO_ENABLE_INTERPRETERS = \"1\"");
                    println!("  RUNTIMO_ENABLE_NETWORK = \"1\"");
                }
            }
            ConfigAction::Dal { level } => {
                if let Some(new_level) = level {
                    let upper = new_level.to_uppercase();
                    if !matches!(upper.as_str(), "A" | "B" | "C" | "D" | "E") {
                        eprintln!(
                            "Invalid DAL level: {}. Must be A, B, C, D, or E.",
                            new_level
                        );
                        std::process::exit(1);
                    }
                    let mut config = RuntimoConfig::load();
                    config.dal = Some(upper.clone());
                    config.save().map_err(|e| format!("Save failed: {}", e))?;
                    println!("DAL set to {} in config file.", upper);
                    println!("Note: RUNTIMO_DAL env var (if set) still takes precedence.");
                } else {
                    let current = RuntimoConfig::get_dal();
                    let source = if std::env::var("RUNTIMO_DAL").is_ok() {
                        "env var (RUNTIMO_DAL)"
                    } else {
                        let config = RuntimoConfig::load();
                        if config.dal.is_some() {
                            "config file"
                        } else {
                            "default"
                        }
                    };
                    println!("Current DAL: {} (source: {})", current, source);
                }
            }
            ConfigAction::Init {
                profile,
                force,
                minimal,
                path,
            } => {
                let effective_profile = if minimal {
                    Some("minimal".to_string())
                } else {
                    profile
                };
                let prof_ref = effective_profile.as_deref();
                let target = path.as_deref();
                match RuntimoConfig::init_at(target, prof_ref, force) {
                    Ok(written) => {
                        println!("Config written to {}", written.display());
                        println!("Next: runtimo config show");
                    }
                    Err(e) => {
                        if e.contains("already exists") {
                            eprintln!("{}", e);
                        } else {
                            eprintln!("Config init failed: {}", e);
                        }
                        std::process::exit(1);
                    }
                }
            }
        },
        Commands::Session { command } => match command {
            SessionCommand::Run {
                prompt_file,
                session,
                max_steps,
                max_seconds,
                on_failure,
                dry_run,
                via_daemon,
            } => {
                // Inherit bounded policy from frozen ResolvedConfig.
                let effective_max_steps = max_steps.unwrap_or(resolved.session_max);
                let effective_max_seconds = max_seconds.unwrap_or(resolved.session_timeout);
                let effective_on_failure = on_failure
                    .as_deref()
                    .unwrap_or(&resolved.session_on_limit)
                    .to_lowercase();
                if effective_on_failure != "continue" && effective_on_failure != "stop" {
                    eprintln!(
                        "Invalid --on-failure '{}': must be continue|stop",
                        effective_on_failure
                    );
                    std::process::exit(1);
                }
                if effective_max_steps == 0 {
                    eprintln!("--max-steps must be >0");
                    std::process::exit(1);
                }

                let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;

                // Parse prompt file (validates size, steps>0, capability exists, traversal).
                let steps = match session_parser::parse_prompt_file(&prompt_file, &reg) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Prompt parse failed: {}", e);
                        std::process::exit(1);
                    }
                };

                if u32::try_from(steps.len()).unwrap_or(u32::MAX) > effective_max_steps {
                    eprintln!(
                        "Prompt has {} steps but --max-steps is {}",
                        steps.len(),
                        effective_max_steps
                    );
                    std::process::exit(1);
                }

                // --dry-run validates only.
                if dry_run {
                    if base_mode.is_json() {
                        let out = serde_json::json!({
                            "dry_run": true,
                            "steps": steps.len(),
                            "max_steps": effective_max_steps,
                            "max_seconds": effective_max_seconds,
                            "on_failure": effective_on_failure,
                            "profile": resolved.profile,
                            "wal_mode": resolved.wal_mode,
                        });
                        println!("{}", serde_json::to_string_pretty(&out).unwrap());
                    } else if !base_mode.is_quiet() {
                        println!(
                            "dry-run: prompt '{}' valid ({} steps, max_steps={}, max_seconds={}, on_failure={})",
                            prompt_file.display(),
                            steps.len(),
                            effective_max_steps,
                            effective_max_seconds,
                            effective_on_failure
                        );
                    }
                    return Ok(());
                }

                // Resolve sessions directory (honors RUNTIMO_SESSIONS_DIR).
                let sdir = session_run::sessions_dir();
                std::fs::create_dir_all(&sdir)
                    .map_err(|e| format!("create sessions dir: {}", e))?;

                // Create or resume session by name/id.
                let session_id = if let Some(ref name) = session {
                    if let Some(existing) = session_run::find_session_by_name_or_id(&sdir, name) {
                        existing.id
                    } else {
                        // Create new with this name.
                        let mut mgr = runtimo_core::session::SessionManager::new(sdir.clone())
                            .map_err(|e| format!("SessionManager: {}", e))?;
                        let s = mgr
                            .create_session(Some(name))
                            .map_err(|e| format!("create_session: {}", e))?;
                        s.id
                    }
                } else {
                    let mut mgr = runtimo_core::session::SessionManager::new(sdir.clone())
                        .map_err(|e| format!("SessionManager: {}", e))?;
                    let s = mgr
                        .create_session(None)
                        .map_err(|e| format!("create_session: {}", e))?;
                    s.id
                };

                // Load session for display (need its name/id).
                let mgr_ro = runtimo_core::session::SessionManager::new(sdir.clone())
                    .map_err(|e| format!("SessionManager: {}", e))?;
                let sess = mgr_ro
                    .load_session(&session_id)
                    .map_err(|e| format!("load_session: {}", e))?;

                if !base_mode.is_quiet() {
                    if base_mode.is_json() {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "session_id": sess.id,
                                "session_name": sess.name,
                                "profile": resolved.profile,
                                "wal_mode": resolved.wal_mode,
                                "backup_enabled": resolved.backup_enabled,
                                "steps": steps.len(),
                            }))
                            .unwrap()
                        );
                    } else {
                        println!(
                            "Session {} (name: {}) — profile={}, wal_mode={}, steps={}",
                            sess.id,
                            sess.name.as_deref().unwrap_or("-"),
                            resolved.profile,
                            resolved.wal_mode,
                            steps.len()
                        );
                    }
                }

                let start = std::time::Instant::now();
                let mut executed: usize = 0;
                let mut had_failure = false;
                let mut terminated = false;

                for (idx, step) in steps.iter().enumerate() {
                    let step_no = idx + 1;

                    // Overall time guard (bounded).
                    if start.elapsed().as_secs() > effective_max_seconds {
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({"session_id": session_id, "step": step_no, "total": steps.len(), "error": "max-seconds exceeded", "terminated": true}),
                            &format!(
                                "Session {} exceeded --max-seconds {}s at step {}/{} — terminating",
                                session_id,
                                effective_max_seconds,
                                step_no,
                                steps.len()
                            ),
                        );
                        had_failure = true;
                        terminated = true;
                        break;
                    }

                    // Resource guard per step.
                    let guard = runtimo_core::LlmoSafeGuard::new();
                    if let Err(e) = guard.check() {
                        let msg = format!(
                            "resource guard tripped at step {}/{}: {}",
                            step_no,
                            steps.len(),
                            e
                        );
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({"step": step_no, "total": steps.len(), "capability": step.capability, "error": msg}),
                            &msg,
                        );
                        had_failure = true;
                        if effective_on_failure == "stop" {
                            terminated = true;
                            break;
                        } else {
                            continue;
                        }
                    }

                    // Dangerous command + network gating per step (ShellExec).
                    if step.capability == "ShellExec" {
                        if let Some(cmd) = step.args.get("cmd").and_then(|v| v.as_str()) {
                            if let Some(reason) = is_dangerous_command(cmd) {
                                let msg = format!(
                                    "step {}/{} dangerous command blocked: {}",
                                    step_no,
                                    steps.len(),
                                    reason
                                );
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({"step": step_no, "total": steps.len(), "capability": step.capability, "error": msg}),
                                    &msg,
                                );
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                } else {
                                    continue;
                                }
                            }
                            if !network_enabled() && is_network_command(cmd) {
                                let msg = format!(
                                    "step {}/{} network command blocked at step {} — set RUNTIMO_ENABLE_NETWORK=1 to enable",
                                    step_no,
                                    steps.len(),
                                    step_no
                                );
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({"step": step_no, "total": steps.len(), "capability": step.capability, "error": msg}),
                                    &msg,
                                );
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                } else {
                                    continue;
                                }
                            }
                        }
                    }

                    // Acquire concurrency slot (reuse run's guard).
                    if !acquire_cli_slot() {
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({"step": step_no, "total": steps.len(), "error": "too many concurrent CLI runs"}),
                            &format!(
                                "Too many concurrent CLI runs (max {}) at step {}/{}",
                                MAX_CLI_CONCURRENT,
                                step_no,
                                steps.len()
                            ),
                        );
                        had_failure = true;
                        if effective_on_failure == "stop" {
                            terminated = true;
                            break;
                        } else {
                            continue;
                        }
                    }

                    // Execute step: local or via-daemon stub.
                    let result: Result<runtimo_core::executor::ExecutionResult, String> =
                        if via_daemon {
                            // Stub: dispatch via daemon RPC.
                            if let Err(e) = ensure_daemon_running() {
                                release_cli_slot();
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({"step": step_no, "total": steps.len(), "error": format!("failed to start daemon: {}", e)}),
                                    &format!(
                                        "via-daemon step {}/{} failed to start daemon: {}",
                                        step_no,
                                        steps.len(),
                                        e
                                    ),
                                );
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                } else {
                                    continue;
                                }
                            }
                            let params = serde_json::json!({
                                "capability": step.capability,
                                "args": step.args,
                                "dry_run": false,
                                "working_dir": std::env::current_dir().unwrap_or_default().to_string_lossy(),
                            });
                            match send_rpc("dispatch", params) {
                                Ok(v) => {
                                    // No forgery: poll the daemon for the real
                                    // terminal status instead of assuming success.
                                    // The WAL audit stays daemon-side; snapshots
                                    // below are local placeholders with cleared
                                    // caches so before/after always differ.
                                    let jid =
                                        v.get("job_id").and_then(|x| x.as_str()).unwrap_or("?");
                                    // Track job_id in session even when dispatched via daemon (stub).
                                    if let Ok(mut mgr) = runtimo_core::session::SessionManager::new(
                                        session_run::sessions_dir(),
                                    ) {
                                        let _ = mgr.add_job(&session_id, jid);
                                    }
                                    let remaining = effective_max_seconds
                                        .saturating_sub(start.elapsed().as_secs());
                                    let (success, err_msg, wal_seq) =
                                        poll_dispatched_step(jid, remaining);
                                    let out = match err_msg {
                                        None => runtimo_core::capability::Output::ok(format!(
                                            "dispatched via daemon: {}",
                                            jid
                                        )),
                                        Some(err) => runtimo_core::capability::Output::error(
                                            format!("via-daemon step failed: {}", err),
                                            err,
                                        ),
                                    };
                                    runtimo_core::Telemetry::clear_cache();
                                    let tel_before = if resolved.telemetry_enabled {
                                        runtimo_core::Telemetry::capture_lightweight()
                                    } else {
                                        runtimo_core::Telemetry::empty()
                                    };
                                    runtimo_core::Telemetry::clear_lightweight_cache();
                                    runtimo_core::ProcessSnapshot::clear_cache();
                                    let tel_after = if resolved.telemetry_enabled {
                                        runtimo_core::Telemetry::capture_lightweight()
                                    } else {
                                        runtimo_core::Telemetry::empty()
                                    };
                                    let proc_before =
                                        runtimo_core::ProcessSnapshot::capture().summary;
                                    runtimo_core::ProcessSnapshot::clear_cache();
                                    let proc_after =
                                        runtimo_core::ProcessSnapshot::capture().summary;
                                    Ok(runtimo_core::executor::ExecutionResult {
                                        job_id: jid.to_string(),
                                        capability: step.capability.clone(),
                                        success,
                                        output: out,
                                        telemetry_before: tel_before,
                                        telemetry_after: tel_after,
                                        process_before: proc_before,
                                        process_after: proc_after,
                                        wal_seq,
                                    })
                                }
                                Err(e) => Err(format!("via-daemon dispatch failed: {}", e)),
                            }
                        } else {
                            // Local deterministic execution with session binding.
                            let Some(cap) = reg.get(&step.capability) else {
                                // Unknown capability — treat as step failure per on_failure policy.
                                let msg = format!("unknown capability '{}'", step.capability);
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({"step": step_no, "total": steps.len(), "capability": step.capability, "error": msg}),
                                    &format!("step {}/{} {}", step_no, steps.len(), msg),
                                );
                                release_cli_slot();
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                }
                                continue;
                            };
                            let timeout =
                                RuntimoConfig::get_capability_timeout(&step.capability, 30);
                            let res = execute_with_telemetry_and_session(
                                cap,
                                &step.args,
                                false,
                                &wal_path(),
                                Some(&session_id),
                                None,
                                timeout,
                            )
                            .map_err(|e| format!("{}", e))?;
                            Ok(res)
                        };

                    release_cli_slot();

                    match result {
                        Ok(exec) => {
                            executed += 1;
                            if !exec.success {
                                had_failure = true;
                                // Stream failure.
                                if base_mode.is_json() {
                                    println!(
                                        "{}",
                                        serde_json::to_string_pretty(&serde_json::json!({
                                            "step": step_no,
                                            "total": steps.len(),
                                            "capability": step.capability,
                                            "success": false,
                                            "output": exec.output,
                                            "job_id": exec.job_id,
                                        }))
                                        .unwrap()
                                    );
                                } else if !base_mode.is_quiet() {
                                    eprintln!(
                                        "step {}/{} {} failed: {}",
                                        step_no,
                                        steps.len(),
                                        step.capability,
                                        exec.output.output
                                    );
                                }
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                }
                            } else if base_mode.is_json() {
                                println!("{}", serde_json::to_string_pretty(&exec.output).unwrap());
                            } else if !base_mode.is_quiet() {
                                let rendered = base_mode.render_text(&exec.output.output);
                                if rendered.trim().is_empty() {
                                    println!(
                                        "step {}/{} {}: ok",
                                        step_no,
                                        steps.len(),
                                        step.capability
                                    );
                                } else {
                                    println!(
                                        "step {}/{} {}: {}",
                                        step_no,
                                        steps.len(),
                                        step.capability,
                                        rendered
                                    );
                                }
                                if let Some(ref data) = exec.output.data {
                                    let text = if let Some(s) = data.as_str() {
                                        s.to_string()
                                    } else {
                                        data.to_string()
                                    };
                                    if !text.trim().is_empty() && text != "null" {
                                        let r2 = base_mode.render_text(&text);
                                        if !r2.trim().is_empty() {
                                            println!("{}", r2);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            executed += 1;
                            had_failure = true;
                            emit_session_note(
                                &base_mode,
                                serde_json::json!({"step": step_no, "total": steps.len(), "capability": step.capability, "error": e}),
                                &format!(
                                    "step {}/{} {} error: {}",
                                    step_no,
                                    steps.len(),
                                    step.capability,
                                    e
                                ),
                            );
                            if effective_on_failure == "stop" {
                                terminated = true;
                                break;
                            }
                        }
                    }
                }

                // Mark terminal status deterministically.
                let final_status = if terminated || (had_failure && effective_on_failure == "stop")
                {
                    runtimo_core::session::SessionStatus::Terminated
                } else if had_failure && effective_on_failure == "continue" {
                    // Completed even though some steps failed — the session itself completed.
                    runtimo_core::session::SessionStatus::Completed
                } else {
                    runtimo_core::session::SessionStatus::Completed
                };
                if let Err(e) = session_run::update_session_status(&sdir, &session_id, final_status)
                {
                    emit_session_note(
                        &base_mode,
                        serde_json::json!({"session_id": session_id, "error": format!("failed to update session status: {}", e)}),
                        &format!("Failed to update session status: {}", e),
                    );
                }

                // Summary via OutputMode.
                if base_mode.is_json() {
                    #[allow(clippy::redundant_clone)]
                    // sdir cloned for json branch so else-if can still move sdir; removing clone would move in one branch and break the other
                    let mgr = runtimo_core::session::SessionManager::new(sdir.clone())
                        .map_err(|e| format!("SessionManager: {}", e))?;
                    let final_sess = mgr
                        .load_session(&session_id)
                        .map_err(|e| format!("{}", e))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "session_id": final_sess.id,
                            "status": format!("{:?}", final_sess.status),
                            "job_ids": final_sess.job_ids,
                            "executed": executed,
                            "total": steps.len(),
                        }))
                        .unwrap()
                    );
                } else if !base_mode.is_quiet() {
                    let mgr = runtimo_core::session::SessionManager::new(sdir).ok();
                    let final_sess = mgr.and_then(|m| m.load_session(&session_id).ok());
                    if let Some(fs) = final_sess {
                        println!(
                            "Session {} status={:?} jobs={}/{} job_ids={:?}",
                            fs.id,
                            fs.status,
                            fs.job_ids.len(),
                            steps.len(),
                            fs.job_ids
                        );
                    }
                }
            }
            SessionCommand::List { json } => {
                let mode = if json {
                    base_mode.with_format("json")
                } else {
                    base_mode
                };
                let sdir = session_run::sessions_dir();
                let mgr = runtimo_core::session::SessionManager::new(sdir)
                    .map_err(|e| format!("SessionManager: {}", e))?;
                let sessions = mgr.list_sessions().map_err(|e| format!("{}", e))?;
                if mode.is_json() {
                    println!("{}", serde_json::to_string_pretty(&sessions).unwrap());
                } else if mode.is_quiet() {
                    // silent
                } else if sessions.is_empty() {
                    println!("No sessions found.");
                } else {
                    let headers = ["ID", "NAME", "STATUS", "JOBS", "UPDATED"];
                    let rows: Vec<Vec<String>> = sessions
                        .iter()
                        .map(|s| {
                            vec![
                                s.id.clone(),
                                s.name.clone().unwrap_or_else(|| "-".to_string()),
                                format!("{:?}", s.status),
                                s.job_ids.len().to_string(),
                                s.updated_at.to_string(),
                            ]
                        })
                        .collect();
                    println!("{}", mode.render_table(&headers, &rows));
                }
            }
            SessionCommand::Show { session_id, json } => {
                let mode = if json {
                    base_mode.with_format("json")
                } else {
                    base_mode
                };
                let sdir = session_run::sessions_dir();
                let mgr = runtimo_core::session::SessionManager::new(sdir)
                    .map_err(|e| format!("SessionManager: {}", e))?;
                let sess = mgr
                    .load_session(&session_id)
                    .map_err(|e| format!("{}", e))?;
                // Pull WAL events for the session's job_ids.
                let wal_events: Vec<Value> = if let Ok(reader) = WalReader::load_all(&wal_path()) {
                    let set: std::collections::HashSet<&String> = sess.job_ids.iter().collect();
                    reader
                        .events()
                        .iter()
                        .filter(|e| set.contains(&e.job_id))
                        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                        .collect()
                } else {
                    Vec::new()
                };
                if mode.is_json() {
                    let out = serde_json::json!({
                        "session": sess,
                        "wal_events": wal_events,
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                } else if mode.is_quiet() {
                    // silent
                } else {
                    let info = format!(
                        "Session {} (name: {})\nStatus: {:?}\nCreated: {}\nUpdated: {}\nJobs ({}): {}\nWAL events: {}",
                        sess.id,
                        sess.name.as_deref().unwrap_or("-"),
                        sess.status,
                        sess.created_at,
                        sess.updated_at,
                        sess.job_ids.len(),
                        if sess.job_ids.is_empty() {
                            "(none)".to_string()
                        } else {
                            sess.job_ids.join(", ")
                        },
                        wal_events.len()
                    );
                    println!("{}", mode.render_text(&info));
                    if !wal_events.is_empty() {
                        let headers = ["SEQ", "EVENT", "JOB_ID"];
                        let rows: Vec<Vec<String>> = wal_events
                            .iter()
                            .map(|v| {
                                vec![
                                    v.get("seq")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        .to_string(),
                                    v.get("type")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("?")
                                        .to_string(),
                                    v.get("job_id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("?")
                                        .to_string(),
                                ]
                            })
                            .collect();
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
            }
        },
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize tests that modify CLI_ACTIVE_JOBS counter.
    /// Without this, concurrent tests fight over the process-global counter.
    static CLI_SLOT_MUTEX: Mutex<()> = Mutex::new(());

    // ── CLI Argument Parsing (GAP 3) ─────────────────────────────────

    #[test]
    fn test_cli_parse_run_command() {
        let args = vec![
            "runtimo",
            "run",
            "-c",
            "FileRead",
            "-a",
            "{\"path\":\"/tmp/test.txt\"}",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run {
                capability,
                args,
                dry_run,
                ..
            } => {
                assert_eq!(capability, "FileRead");
                assert_eq!(args, "{\"path\":\"/tmp/test.txt\"}");
                assert!(!dry_run);
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_run_with_flags() {
        let args = vec![
            "runtimo",
            "run",
            "-c",
            "ShellExec",
            "-a",
            "{\"cmd\":\"echo hello\"}",
            "--dry-run",
            "--json",
            "--timeout",
            "10",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run {
                capability,
                dry_run,
                json,
                quiet,
                timeout,
                ..
            } => {
                assert_eq!(capability, "ShellExec");
                assert!(dry_run);
                assert!(json);
                assert!(!quiet);
                assert_eq!(timeout, Some(10));
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_dispatch_command() {
        let args = vec![
            "runtimo",
            "dispatch",
            "-c",
            "FileWrite",
            "-a",
            "{\"path\":\"/tmp/x.txt\",\"content\":\"bg\"}",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Dispatch {
                capability,
                args,
                args_file: _,
                args_stdin: _,
                dry_run,
            } => {
                assert_eq!(capability, "FileWrite");
                assert!(!dry_run);
                // Verify args was captured (not empty)
                assert!(!args.is_empty(), "Dispatch args should not be empty");
                // Verify args contains the expected content field
                assert!(
                    args.contains("\"content\":\"bg\""),
                    "Args should contain content:bg, got: {}",
                    args
                );
            }
            _ => panic!("Expected Dispatch command"),
        }
    }

    #[test]
    fn test_cli_parse_list_command() {
        let args = vec!["runtimo", "list"];
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.command, Commands::List { .. }));
    }

    #[test]
    fn test_cli_parse_telemetry_command() {
        let args = vec!["runtimo", "telemetry", "--json"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Telemetry { json, verbose } => {
                assert!(json);
                assert!(!verbose);
            }
            _ => panic!("Expected Telemetry command"),
        }
    }

    #[test]
    fn test_cli_parse_telemetry_verbose() {
        let args = vec!["runtimo", "telemetry", "--verbose"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Telemetry { json, verbose } => {
                assert!(!json);
                assert!(verbose);
            }
            _ => panic!("Expected Telemetry command"),
        }
    }

    #[test]
    fn test_cli_parse_invalid_command() {
        let args = vec!["runtimo", "nonexistent_command"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_missing_required_arg() {
        // 'run' requires -c (capability) — should fail without it
        let args = vec!["runtimo", "run"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    // ── MAX_CLI_CONCURRENT Slot Enforcement (GAP 3) ──────────────────

    #[test]
    fn test_acquire_cli_slot_under_limit() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap();
        // Reset counter for test isolation
        CLI_ACTIVE_JOBS.store(0, Ordering::Relaxed);

        let mut successes = 0;
        for _ in 0..MAX_CLI_CONCURRENT {
            if acquire_cli_slot() {
                successes += 1;
            }
        }
        assert_eq!(
            successes, MAX_CLI_CONCURRENT,
            "Should acquire all {} slots",
            MAX_CLI_CONCURRENT
        );

        // Release all
        for _ in 0..MAX_CLI_CONCURRENT {
            release_cli_slot();
        }
    }

    #[test]
    fn test_acquire_cli_slot_over_limit() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap();
        // Reset counter
        CLI_ACTIVE_JOBS.store(0, Ordering::Relaxed);

        // Acquire all slots
        for _ in 0..MAX_CLI_CONCURRENT {
            assert!(acquire_cli_slot(), "Should acquire slot");
        }

        // Next acquisition should fail
        assert!(!acquire_cli_slot(), "Should reject when at limit");

        // Release all
        for _ in 0..MAX_CLI_CONCURRENT {
            release_cli_slot();
        }
    }

    #[test]
    fn test_release_cli_slot_after_acquire() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap();
        CLI_ACTIVE_JOBS.store(0, Ordering::Relaxed);

        assert!(acquire_cli_slot());
        assert_eq!(CLI_ACTIVE_JOBS.load(Ordering::Relaxed), 1);

        release_cli_slot();
        assert_eq!(CLI_ACTIVE_JOBS.load(Ordering::Relaxed), 0);

        // Should be able to acquire again
        assert!(acquire_cli_slot());
        release_cli_slot();
    }

    // ── Flock Coordination (GAP 3) ───────────────────────────────────

    #[test]
    fn test_acquire_daemon_lock_creates_file() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap(); // serialize env var access
                                                     // Override XDG_DATA_HOME to use temp dir
        let tmp = std::env::temp_dir().join("runtimo_cli_lock_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_DATA_HOME", &tmp);

        let result = acquire_daemon_lock();
        // Should succeed since no other process holds the lock (NB mode)
        assert!(
            result.is_ok(),
            "acquire_daemon_lock failed: {:?}",
            result.err()
        );

        let lock_path = daemon_lock_path();
        assert!(
            lock_path.exists(),
            "Lock file should exist at {}",
            lock_path.display()
        );

        // Drop the lock to release it
        drop(result.unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_daemon_lock_path_format() {
        let lock_path = daemon_lock_path();
        // Should end with daemon.lock
        let path_str = lock_path.to_string_lossy();
        assert!(
            path_str.ends_with("daemon.lock"),
            "Lock path should end with daemon.lock: {}",
            path_str
        );
        assert!(
            path_str.contains("runtimo"),
            "Lock path should contain runtimo: {}",
            path_str
        );
    }

    #[test]
    fn test_daemon_socket_path_format() {
        let sock_path = daemon_socket();
        let path_str = sock_path.to_string_lossy();
        assert!(
            path_str.ends_with("runtimo.sock"),
            "Socket should end with runtimo.sock: {}",
            path_str
        );
    }
}
