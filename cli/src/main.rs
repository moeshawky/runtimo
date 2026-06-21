//! runtimo CLI — Agent capability runtime with background dispatch

mod format;

use clap::{Parser, Subcommand};
use format::wall_to_markdown;
use runtimo_core::{
    capabilities::{is_dangerous_command, FileRead, FileWrite, GitExec, Kill, ShellExec, Undo},
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
    after_help = "USAGE:\n runtimo run -c <Capability> -a '<json>'\n runtimo dispatch -c <Capability> -a '<json>'\n runtimo jobs\n runtimo wait -j <job_id>\n runtimo list\n runtimo logs\n runtimo telemetry\n runtimo processes\n\nCAPABILITIES:\n FileRead  Read file. Path validated. No dirs, no traversal.\n FileWrite Write file. Auto-backup for undo. Append mode ok.\n ShellExec Exec via sh -c. Blocks rm, shutdown, chmod, mkfs, dd, iptables, fork bombs, env dumpers, network tools (opt-in). See `runtimo list` for full blocklist.\n GitExec   Git ops: clone|pull|commit|revert|clean|status.\n Kill      Kill process by PID. Protected: init, kthreadd, self.\n Undo      Restore from backup. Find job IDs with `runtimo jobs` or `runtimo logs`.\n\nTIP: Use `runtimo run -c <Cap> --schema` to see the JSON args a capability expects.\nTIP: Use `runtimo list --schemas` to see all schemas at once.\nTIP: ShellExec timeout range is 1–300 seconds (default: 30).\n\nDaemon starts on first dispatch if runtimo-daemon is installed.",
    version
)]
struct Cli {
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
        /// Execution timeout in seconds (1–300, default: 30)
        #[arg(long, default_value = "30", value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout: u64,
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
    /// Check job status (local or dispatched)
    #[command(
        about = "Check job status",
        after_help = "EXAMPLES:\n runtimo status             # all jobs\n runtimo status -j abc123   # specific job\n runtimo status -oj         # JSON output"
    )]
    Status {
        /// Job ID to check (omit to list all)
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Output raw JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// List recent jobs from WAL (local + dispatched)
    #[command(
        about = "List recent jobs",
        after_help = "EXAMPLES:\n runtimo jobs\n runtimo jobs --limit 5\n runtimo jobs --json"
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
    #[command(about = "Manage configuration")]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    AllowedPaths {
        #[command(subcommand)]
        subaction: AllowedPathsAction,
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
        return Err(
            "Cannot use both --args-file and --args-stdin simultaneously".to_string(),
        );
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

// ── Main ────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines, clippy::indexing_slicing)] // JSON Value indexing is intentional
fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

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
            let cap = reg
                .get(&capability)
                .ok_or_else(|| format!("Capability not found: {}. Use `runtimo list` to see available capabilities.", capability))?;
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
            let result = execute_with_telemetry_and_session(
                cap,
                &args_val,
                dry_run,
                &wal_path(),
                None,
                None,
                timeout,
            )
            .map_err(|e| format!("{}", e));
            release_cli_slot();
            let result = result?;
            if !result.success {
                eprintln!("{}", result.output.output);
                std::process::exit(1);
            }
            let output = result.output;
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if !quiet {
                println!("{}", output.output);
                if let Some(ref data) = output.data {
                    let text = if let Some(s) = data.as_str() {
                        s.to_string()
                    } else {
                        data.to_string()
                    };
                    println!("{}", wall_to_markdown(&text));
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
                        if let Ok(reader) = WalReader::load(&wal_path()) {
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
                        if let Ok(reader) = WalReader::load(&wal_path()) {
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
            if json {
                let caps: Vec<Value> = reg.list().iter().map(|name| {
                    if let Some(cap) = reg.get(name) {
                        serde_json::json!({
                            "name": name,
                            "description": cap.description(),
                            "schema": if schemas { Some(cap.schema().to_string()) } else { None },
                        })
                    } else {
                        Value::Null
                    }
                }).filter(|v| !v.is_null()).collect();
                println!("{}", serde_json::to_string_pretty(&caps)?);
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
            if let Some(jid) = job_id {
                // Try daemon RPC first
                if let Ok(result) = send_rpc("status", serde_json::json!({ "job_id": &jid })) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        println!(
                            "Job: {}  Status: {}  Capability: {}",
                            result.get("job_id").and_then(|v| v.as_str()).unwrap_or("?"),
                            result.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                            result
                                .get("capability")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                        );
                    }
                    return Ok(());
                }

                // Fallback to WAL
                if let Ok(reader) = WalReader::load(&wal_path()) {
                    let events = reader.events();
                    let by_job: Vec<_> = events.iter().filter(|e| e.job_id == jid).collect();
                    if by_job.is_empty() {
                        println!("Job not found: {}", jid);
                    } else {
                        for e in &by_job {
                            println!(
                                "{:?}  {:>15}  {:?}",
                                e.event_type,
                                e.capability.as_deref().unwrap_or("-"),
                                e.ts
                            );
                        }
                    }
                } else {
                    println!("Cannot read WAL");
                }
            } else {
                // List all jobs via daemon
                if let Ok(result) = send_rpc("jobs", serde_json::json!({ "limit": 50 })) {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        let jobs = result["jobs"].as_array().cloned().unwrap_or_default();
                        for job in &jobs {
                            println!(
                                "  {}  {:>8}  {}",
                                job["job_id"].as_str().unwrap_or("?"),
                                job["status"].as_str().unwrap_or("?"),
                                job["capability"].as_str().unwrap_or("?")
                            );
                        }
                    }
                } else {
                    // Fallback to WAL
                    if let Ok(reader) = WalReader::load(&wal_path()) {
                        let events = reader.events();
                        let mut seen: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        for e in events.iter().rev() {
                            if seen.contains(&e.job_id) {
                                continue;
                            }
                            seen.insert(&e.job_id);
                            println!(
                                "{:?}  {}  {:?}  {}",
                                e.event_type,
                                e.job_id,
                                e.capability.as_deref().unwrap_or("-"),
                                e.ts
                            );
                        }
                    }
                }
            }
        }

        Commands::Jobs { limit, json } => {
            // Try daemon RPC first
            let result = send_rpc("jobs", serde_json::json!({ "limit": limit }));
            match result {
                Ok(data) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&data)?);
                    } else {
                        let jobs = data["jobs"].as_array().cloned().unwrap_or_default();
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else {
                            let md_lines: Vec<String> = jobs
                                .iter()
                                .map(|j| {
                                    let jid = j["job_id"].as_str().unwrap_or("?");
                                    let cap = j["capability"].as_str().unwrap_or("?");
                                    let status = j["status"].as_str().unwrap_or("?");
                                    let icon = match status {
                                        "running" => "🔄",
                                        "completed" => "✅",
                                        "failed" => "❌",
                                        _ => "❓",
                                    };
                                    format!("- {} **{}**  {}  {}", icon, jid, cap, status)
                                })
                                .collect();
                            println!("## Recent Jobs ({})\n{}", jobs.len(), md_lines.join("\n"));
                        }
                    }
                }
                Err(_) => {
                    // Fallback to WAL
                    if let Ok(reader) = WalReader::load(&wal_path()) {
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
                            jobs.push(serde_json::json!({
                                "job_id": e.job_id,
                                "capability": e.capability,
                                "status": match e.event_type {
                                    runtimo_core::WalEventType::JobStarted => "started",
                                    runtimo_core::WalEventType::JobCompleted => "completed",
                                    runtimo_core::WalEventType::JobFailed => "failed",
                                    _ => "unknown",
                                },
                                "started_at": e.ts,
                            }));
                        }
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else {
                            for j in &jobs {
                                let jid = j["job_id"].as_str().unwrap_or("?");
                                let cap = j["capability"].as_str().unwrap_or("?");
                                let status = j["status"].as_str().unwrap_or("?");
                                let icon = match status {
                                    "running" | "started" => "🔄",
                                    "completed" => "✅",
                                    "failed" => "❌",
                                    _ => "❓",
                                };
                                println!("  {} {}  {:>15}  {}", icon, jid, cap, status);
                            }
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
            // Try daemon RPC first
            let mut params = serde_json::json!({ "limit": limit });
            if let Some(ref jid) = job_id {
                params["job_id"] = serde_json::json!(jid);
            }
            if let Ok(result) = send_rpc("logs", params) {
                if json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    let events = result["events"].as_array().cloned().unwrap_or_default();
                    for e in &events {
                        let ts = e["ts"].as_u64().unwrap_or(0);
                        let et = e["event_type"].as_str().unwrap_or("?");
                        let jid = e["job_id"].as_str().unwrap_or("?");
                        let cap = e["capability"].as_str().unwrap_or("-");
                        println!("{:?}  {}  {}  {:>15}", et, ts, jid, cap);
                    }
                }
            } else if let Ok(reader) = WalReader::load(&wal_path()) {
                let events = reader.events();
                let filtered: Vec<_> = if let Some(ref jid) = job_id {
                    events.iter().filter(|e| e.job_id == *jid).collect()
                } else {
                    events.iter().collect()
                };
                let recent: Vec<_> = filtered.iter().rev().take(limit).rev().collect();
                if json {
                    println!("{}", serde_json::to_string_pretty(&recent)?);
                } else {
                    for e in &recent {
                        println!(
                            "{:?}  {}  {:?}  {}",
                            e.event_type,
                            e.job_id,
                            e.capability.as_deref().unwrap_or("-"),
                            e.ts
                        );
                    }
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
            let tel = Telemetry::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&tel)?);
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
                println!("{}", wall_to_markdown(&text));
            }
        }

        Commands::Processes { json } => {
            let snap = ProcessSnapshot::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
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
                println!("{}", wall_to_markdown(&text));
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
                assert_eq!(timeout, 10);
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
