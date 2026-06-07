//! runtimo CLI — Agent capability runtime with background dispatch

mod format;

use clap::{Parser, Subcommand};
use format::wall_to_markdown;
use runtimo_core::{
    capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo},
    execute_with_telemetry_and_session, CapabilityRegistry, ProcessSnapshot, RuntimoConfig,
    Telemetry, WalReader,
};
use serde_json::Value;
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "runtimo",
    about = "capability runtime with telemetry, WAL, process tracking, and background dispatch",
    long_about = "runtimo — capability runtime with telemetry, WAL, and process tracking\n\n\
Every exec: telemetry + process snapshot + WAL audit\n\
Background: dispatch jobs to daemon, check status later",
    after_help = "USAGE:\n runtimo run -c <Capability> -a '<json>'\n runtimo dispatch -c <Capability> -a '<json>'\n runtimo jobs\n runtimo wait -j <job_id>\n runtimo list\n runtimo logs\n runtimo telemetry\n runtimo processes\n\nCAPABILITIES:\n FileRead Read file. Path validated.\n FileWrite Write file. Auto-backup for undo.\n ShellExec Exec via sh -c. Dangerous cmds blocked.\n GitExec Git ops: clone|pull|commit|revert|clean|status.\n Kill Kill PID. Protected: init, kthreadd, self.\n Undo Restore from backup. Use `runtimo logs` to find job IDs.\n\nDaemon auto-starts on first dispatch.",
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
        after_help = "CAPABILITY HELP:\n runtimo run -c <Cap> --schema\n\nEXAMPLES:\n runtimo run -c FileRead -a '{\"path\":\"/etc/hostname\"}'\n runtimo run -c ShellExec -a '{\"cmd\":\"uptime\"}'"
    )]
    Run {
        #[arg(short = 'c', long)]
        capability: String,
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'j', long)]
        json: bool,
        #[arg(short = 'q', long)]
        quiet: bool,
        #[arg(long)]
        schema: bool,
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// Dispatch job to background daemon (returns immediately)
    #[command(
        about = "Dispatch job to background daemon (starts daemon automatically if needed)",
        after_help = "EXAMPLES:\n runtimo dispatch -c ShellExec -a '{\"cmd\":\"sleep 30\"}'\n runtimo dispatch -c FileWrite -a '{\"path\":\"/tmp/x.txt\",\"content\":\"bg\"}'"
    )]
    Dispatch {
        #[arg(short = 'c', long)]
        capability: String,
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Wait for a dispatched job to complete
    #[command(
        about = "Wait for a dispatched job",
        after_help = "EXAMPLES:\n runtimo wait -j abc123\n runtimo wait -j abc123 --timeout 60"
    )]
    Wait {
        #[arg(short = 'j', long)]
        job_id: String,
        #[arg(long, default_value = "0")]
        timeout: u64,
    },
    /// List available capabilities
    #[command(
        about = "List capabilities",
        after_help = "Use --schemas to see JSON argument schemas."
    )]
    List {
        #[arg(long)]
        schemas: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Check job status
    #[command(about = "Check job status")]
    Status {
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// List recent jobs (queriable job history)
    #[command(
        about = "List recent jobs",
        after_help = "EXAMPLES:\n runtimo jobs\n runtimo jobs --limit 5\n runtimo jobs --json"
    )]
    Jobs {
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        #[arg(short = 'j', long)]
        json: bool,
    },
    #[command(about = "View WAL logs")]
    Logs {
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        #[arg(short = 'o', long)]
        json: bool,
    },
    #[command(
        about = "Undo a completed job",
        after_help = "Find job IDs with `runtimo jobs` or `runtimo logs`."
    )]
    Undo {
        #[arg(short = 'j', long)]
        job_id: String,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(about = "Print system telemetry")]
    Telemetry {
        #[arg(short = 'j', long)]
        json: bool,
    },
    #[command(about = "Print process snapshot")]
    Processes {
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

fn wal_path() -> PathBuf {
    runtimo_core::utils::wal_path()
}
fn backup_dir() -> PathBuf {
    runtimo_core::utils::backup_dir()
}

fn make_registry() -> CapabilityRegistry {
    let mut reg = CapabilityRegistry::new();
    reg.register(FileRead);
    #[allow(clippy::expect_used)] // BUG-4: make_registry should return Result
    reg.register(
        FileWrite::new(backup_dir()).expect("BUG-4: make_registry should return Result, tracked"),
    );
    #[allow(clippy::expect_used)] // GitExec construction failure should be propagated
    reg.register(GitExec::new(backup_dir()).expect("Failed to create GitExec capability"));
    reg.register(ShellExec);
    reg.register(Kill);
    reg.register(Undo);
    reg
}

// Concurrency control for CLI run — mirrors daemon's MAX_CONCURRENT_JOBS = 16
const MAX_CLI_CONCURRENT: usize = 16;
static CLI_ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);

fn acquire_cli_slot() -> bool {
    let current = CLI_ACTIVE_JOBS.fetch_add(1, Ordering::AcqRel);
    if current >= MAX_CLI_CONCURRENT {
        CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}

fn release_cli_slot() {
    CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
}

// ── Daemon client ───────────────────────────────────────────────────────────

fn daemon_socket() -> PathBuf {
    runtimo_core::utils::data_dir().join("runtimo.sock")
}

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

fn daemon_lock_path() -> PathBuf {
    runtimo_core::utils::data_dir().join("daemon.lock")
}

fn acquire_daemon_lock() -> Result<File, String> {
    use libc::flock;
    let lock_path = daemon_lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create lock dir: {}", e))?;
    }
    let file = File::create(&lock_path).map_err(|e| format!("Failed to create lock file: {}", e))?;
    // Try to acquire exclusive non-blocking lock using flock
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid file descriptor from File::create; LOCK_EX | LOCK_NB are valid flock flags
    let result = unsafe { flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err("Another process is starting the daemon".to_string());
    }
    Ok(file)
}

fn daemon_is_running() -> bool {
    UnixStream::connect(daemon_socket()).is_ok()
}

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
        .checked_add(Duration::from_secs(10))
        .unwrap_or_else(|| std::time::Instant::now() + Duration::from_secs(10));
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
                "Daemon started but did not become ready within 10s".into()
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

    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).map_err(|e| format!("Read: {}", e))?;
    if n == 0 {
        return Err("Daemon closed connection".into());
    }

    let resp_str = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
    let last_line = resp_str.lines().last().unwrap_or("");
    let resp: Value = serde_json::from_str(last_line).map_err(|e| format!("JSON parse: {}", e))?;

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
            dry_run,
            json,
            quiet,
            schema,
            timeout,
        } => {
            let reg = make_registry();
            if schema {
                if let Some(cap) = reg.get(&capability) {
                    println!("{}", cap.schema());
                } else {
                    eprintln!("Capability not found: {}", capability);
                    std::process::exit(1);
                }
                return Ok(());
            }
            let cap = reg
                .get(&capability)
                .ok_or_else(|| format!("Capability not found: {}", capability))?;
            let args_val: Value =
                serde_json::from_str(&args).map_err(|e| format!("Invalid JSON args: {}", e))?;
            if let Err(e) = cap.validate(&args_val) {
                eprintln!("Validation failed: {}", e);
                std::process::exit(1);
            }
            // Acquire concurrency slot (mirrors daemon's MAX_CONCURRENT_JOBS)
            if !acquire_cli_slot() {
                eprintln!("Too many concurrent CLI runs (max {}). Try again later.", MAX_CLI_CONCURRENT);
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
            let output = result.output;
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if !quiet {
                println!("{}", output.message.as_deref().unwrap_or("ok"));
                if !output.data.is_null() {
                    let text = if let Some(s) = output.data.as_str() {
                        s.to_string()
                    } else {
                        output.data.to_string()
                    };
                    println!("{}", wall_to_markdown(&text));
                }
            }
        }

        Commands::Dispatch {
            capability,
            args,
            dry_run,
        } => {
            if let Err(e) = ensure_daemon_running() {
                eprintln!("Cannot dispatch: {}", e);
                std::process::exit(1);
            }
            let args_val: Value =
                serde_json::from_str(&args).map_err(|e| format!("Invalid JSON args: {}", e))?;
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
                    eprintln!("Dispatch failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::Wait { job_id, timeout } => {
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
            let reg = make_registry();
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
                        let mut seen: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for e in events.iter().rev() {
                            if seen.contains(&e.job_id) {
                                continue;
                            }
                            seen.insert(e.job_id.clone());
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
                        let mut seen: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for e in events.iter().rev() {
                            if seen.contains(&e.job_id) {
                                continue;
                            }
                            if jobs.len() >= limit {
                                break;
                            }
                            seen.insert(e.job_id.clone());
                            jobs.push(serde_json::json!({
                                "job_id": e.job_id,
                                "capability": e.capability,
                                "status": match e.event_type {
                                    runtimo_core::WalEventType::JobStarted => "started",
                                    runtimo_core::WalEventType::JobCompleted => "completed",
                                    runtimo_core::WalEventType::JobFailed => "failed",
                                    _ => "?",
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
            let reg = make_registry();
            let cap = reg.get("Undo").ok_or("Undo capability not available")?;
            let args = serde_json::json!({ "job_id": job_id });
            let ctx = runtimo_core::Context {
                dry_run,
                job_id: runtimo_core::utils::generate_id(),
                working_dir: std::env::current_dir().unwrap_or_default(),
            };
            let output = cap.execute(&args, &ctx).map_err(|e| format!("{}", e))?;
            println!("{}", output.message.as_deref().unwrap_or("undo completed"));
        }

        Commands::Telemetry { json } => {
            let tel = Telemetry::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&tel)?);
            } else {
                let text = format!(
                    "RUNTIMO TELEMETRY\n\nSystem\nCPU: {}\nRAM: {} total, {} free\nDisk: {} total, {} free ({}% used)\nUptime: {}\nLoad: {}\n\nHardware\nAccelerators: {}\n\nServices\n{}\n\nNetwork\nPublic IP: {}\nTunnel: {}",
                    tel.system.cpu_model,
                    tel.system.ram_total, tel.system.ram_free,
                    tel.system.disk_total, tel.system.disk_free, tel.system.disk_used_percent,
                    tel.system.uptime,
                    tel.system.load_average,
                    if tel.hardware.accelerators.is_empty() { "none".into() } else {
                        tel.hardware.accelerators.iter().map(|a| format!("{}: {}x", a.kind, a.count)).collect::<Vec<_>>().join(", ")
                    },
                    if tel.services.detected_services.is_empty() { "none detected".into() } else {
                        tel.services.detected_services.iter().map(|s| format!("{}: {} {}", s.name, s.version.as_deref().unwrap_or("?"), if s.running { "running" } else { "stopped" })).collect::<Vec<_>>().join(", ")
                    },
                    tel.network.public_ip,
                    if tel.network.tunnel_running { "running" } else { "not running" },
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
                let reg = make_registry();
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
                        Ok(o) => println!("{}", o.message.as_deref().unwrap_or("ok")),
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
