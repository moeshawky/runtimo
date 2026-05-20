//! runtimo CLI — Agent capability runtime

use clap::{Parser, Subcommand};
use runtimo_core::{
    capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo},
    execute_with_telemetry, BackupManager, CapabilityRegistry, ProcessSnapshot, RuntimoConfig,
    Telemetry, WalReader,
};
use std::error::Error;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "runtimo",
    about = "capability runtime for persistent machines",
    long_about = "runtimo — capability runtime for persistent machines\n\n\
Every exec: telemetry + process snapshot + WAL audit",
    after_help = "USAGE:\n runtimo run -c <Capability> -a '<json>'\n runtimo list\n runtimo logs\n runtimo telemetry\n runtimo processes\n\nCAPABILITIES:\n FileRead Read file. Path validated.\n FileWrite Write file. Auto-backup for undo.\n ShellExec Exec via sh -c. Dangerous cmds blocked.\n GitExec Git ops: clone|pull|commit|revert|clean|status.\n Kill Kill PID. Protected: init, kthreadd, self.\n Undo Restore from backup. Use `runtimo logs` to find job IDs.\n\nQUICKSTART:\n runtimo run -c FileRead -a '{\"path\":\"/etc/hostname\"}'\n runtimo run -c ShellExec -a '{\"cmd\":\"uptime\"}'\n\nCONSTRAINTS:\n ShellExec: sh -c mode. Pipes/chaining/vars ok. Blk: mkfs,fdisk,dd,shutdown,rm -rf /\n GitExec: operation + path required.\n All caps: telemetry + WAL audit mandatory.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// exec capability with telemetry
    #[command(
        about = "exec capability with telemetry",
        long_about = "runtimo run -c <Capability> -a '<json>'\n\n\
req: capability (string)\nopt: args (json), dry-run, timeout (30s)\n\n\
ex: runtimo run -c ShellExec -a '{\"cmd\":\"ls | head\"}'",
        after_help = "CAPABILITY HELP:\n runtimo run -c <Cap> --schema\n\n\
EXAMPLES:\n runtimo run -c FileRead -a '{\"path\":\"/etc/hostname\"}'\n runtimo run -c ShellExec -a '{\"cmd\":\"uptime\"}'\n runtimo run -c GitExec -a '{\"operation\":\"status\",\"path\":\"/tmp\"}'",
    )]
    Run {
        /// Capability name (FileRead, FileWrite, ShellExec, Kill, GitExec, Undo)
        #[arg(short = 'c', long)]
        capability: String,
        /// JSON arguments, e.g. '{"path":"/tmp/test.txt"}' or '{"cmd":"uptime"}'
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        /// Validate without executing
        #[arg(long)]
        dry_run: bool,
        /// Output as JSON (machine-readable)
        #[arg(short = 'j', long)]
        json: bool,
        /// Suppress telemetry output (quiet mode)
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Show capability argument schema and exit
        #[arg(long)]
        schema: bool,
        /// Execution timeout in seconds (default: 30)
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// List available capabilities
    #[command(
        about = "List capabilities",
        long_about = "List all registered capabilities with descriptions.",
        after_help = "Use --schemas to see JSON argument schemas for each capability.\nUse --json for machine-readable output.",
    )]
    List {
        /// Show schemas for each capability
        #[arg(long)]
        schemas: bool,
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Check job status
    #[command(
        about = "Check job status",
        long_about = "Show job status from WAL (Write-Ahead Log) history.",
        after_help = "Without --job-id, lists all jobs. With --job-id, shows all events for that job.",
    )]
    Status {
        /// Filter by job ID
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Output as JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// View WAL logs
    #[command(
        about = "View WAL logs",
        long_about = "View the Write-Ahead Log — a sequential record of all capability executions.",
        after_help = "The WAL records every job start, completion, telemetry snapshot, and error.\nUse --job-id to filter. Use --limit to control output size (default: 10).",
    )]
    Logs {
        /// Filter by job ID
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Number of recent events (default: 10)
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Output as JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// Undo a completed job
    #[command(
        about = "Undo a completed job",
        long_about = "Restore files to their state before a job executed, using backups from FileWrite or GitExec.",
        after_help = "Find job IDs with `runtimo logs` or `runtimo status`.\nExample: runtimo undo -j abc123 --dry-run",
    )]
    Undo {
        /// Job ID to undo
        #[arg(short = 'j', long)]
        job_id: String,
        /// Show what will be restored without executing
        #[arg(long)]
        dry_run: bool,
    },
    /// Print system telemetry
    #[command(
        about = "Print system telemetry",
        long_about = "Print hardware info: CPU model, RAM, disk usage, network interfaces, and services.",
    )]
    Telemetry {
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Print process snapshot
    #[command(
        about = "Print process snapshot",
        long_about = "Print running processes: total count, zombies, and top CPU/memory consumers.",
        after_help = "Useful for detecting runaway processes spawned by capabilities.",
    )]
    Processes {
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Manage configuration
    #[command(
        about = "Manage configuration",
        long_about = "Add, remove, or list allowed path prefixes.",
        after_help = "Examples:\n  runtimo config allowed-paths add /srv /opt\n  runtimo config allowed-paths list\n  runtimo config allowed-paths remove /opt",
    )]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Manage allowed path prefixes
    AllowedPaths {
        #[command(subcommand)]
        subaction: AllowedPathsAction,
    },
}

#[derive(Subcommand)]
enum AllowedPathsAction {
    /// Add path prefixes to config
    Add {
        /// Path prefixes to add
        paths: Vec<String>,
    },
    /// Remove path prefixes from config
    Remove {
        /// Path prefixes to remove
        paths: Vec<String>,
    },
    /// List configured path prefixes
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
    reg.register(FileWrite::new(backup_dir()).expect("Failed to create FileWrite capability"));
    reg.register(ShellExec);
    reg.register(Undo);
    reg.register(Kill);
    reg.register(GitExec::new(backup_dir()).expect("Failed to create GitExec capability"));
    reg
}

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
            timeout: _,
        } => {
            let reg = make_registry();
            let cap = reg
                .get(&capability)
                .ok_or_else(|| format!("capability not found: {}", capability))?;

            if schema {
                let schema = cap.schema();
                println!("{}", serde_json::to_string_pretty(&schema)?);
                return Ok(());
            }

            let args: serde_json::Value =
                serde_json::from_str(&args).map_err(|e| format!("invalid JSON args: {}", e))?;

            let wp = wal_path();
            if let Some(parent) = wp.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let result = execute_with_telemetry(cap, &args, dry_run, &wp)?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": result.success,
                        "job_id": result.job_id,
                        "capability": result.capability,
                        "output": result.output,
                        "telemetry_before": result.telemetry_before,
                        "telemetry_after": result.telemetry_after,
                        "process_before": result.process_before,
                        "process_after": result.process_after,
                        "wal_seq": result.wal_seq,
                    }))?
                );
            } else {
                println!(
                    "job: {}  cap: {}  ok: {}",
                    result.job_id, result.capability, result.success
                );
                if let Some(msg) = &result.output.message {
                    println!("  {}", msg);
                }
                if !quiet {
                    println!(
                        "  cpu: {}  ram: {} free  disk: {}",
                        result.telemetry_before.system.cpu_model,
                        result.telemetry_before.system.ram_free,
                        result.telemetry_before.system.disk_used_percent
                    );
                    println!(
                        "  procs: {}  zombies: {}",
                        result.process_before.total_processes, result.process_before.zombie_count
                    );
                }
                println!("  {}", serde_json::to_string_pretty(&result.output.data)?);
            }
            Ok(())
        }

        Commands::List { schemas, json } => {
            let reg = make_registry();
            let caps = reg.list();

            if json {
                if schemas {
                    let mut list = Vec::new();
                    for name in &caps {
                        if let Some(cap) = reg.get(name) {
                            list.push(serde_json::json!({
                                "name": name,
                                "description": cap.description(),
                                "schema": cap.schema(),
                            }));
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    let list: Vec<_> = caps.iter().map(|name| {
                        let desc = reg.get(name).map(|c| c.description()).unwrap_or("");
                        serde_json::json!({ "name": name, "description": desc })
                    }).collect();
                    println!("{}", serde_json::to_string_pretty(&list)?);
                }
            } else if schemas {
                for name in caps {
                    if let Some(cap) = reg.get(name) {
                        println!("{} — {}", name, cap.description());
                        println!("  {}", serde_json::to_string_pretty(&cap.schema())?);
                    }
                }
            } else {
                println!("{} capabilities:", caps.len());
                for c in caps {
                    if let Some(cap) = reg.get(c) {
                        println!("  {:<12} {}", c, cap.description());
                    }
                }
            }
            Ok(())
        }

        Commands::Status { job_id, json } => {
            let wp = wal_path();
            if !wp.exists() {
                if json {
                    println!("{{\"events\": [], \"total\": 0}}");
                } else {
                    println!("no jobs yet");
                }
                return Ok(());
            }
            let reader = WalReader::load(&wp)?;
            match job_id {
                Some(id) => {
                    let events: Vec<_> =
                        reader.events().iter().filter(|e| e.job_id == id).collect();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "job_id": id,
                                "events": events,
                            }))?
                        );
                    } else if events.is_empty() {
                        println!("no events for {}", id);
                    } else {
                        println!("job {} ({} events):", id, events.len());
                        for e in events {
                            println!(
                                "  {:?}  cap={}",
                                e.event_type,
                                e.capability.as_deref().unwrap_or("-")
                            );
                        }
                    }
                }
                None => {
                    let events = reader.events();
                    let mut jobs: std::collections::HashMap<&str, Vec<&runtimo_core::WalEvent>> =
                        std::collections::HashMap::new();
                    for e in events {
                        jobs.entry(&e.job_id).or_default().push(e);
                    }
                    if json {
                        let summary: Vec<_> = jobs
                            .iter()
                            .map(|(jid, evts)| {
                                let last = evts.last().unwrap();
                                serde_json::json!({
                                    "job_id": jid,
                                    "status": last.event_type,
                                    "capability": last.capability,
                                    "event_count": evts.len(),
                                })
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "total_events": events.len(),
                                "jobs": summary,
                            }))?
                        );
                    } else {
                        println!("{} events total:", events.len());
                        for (jid, evts) in &jobs {
                            let last = evts.last().unwrap();
                            println!(
                                "  {}  {:?}  {}",
                                jid,
                                last.event_type,
                                last.capability.as_deref().unwrap_or("-")
                            );
                        }
                    }
                }
            }
            Ok(())
        }

        Commands::Logs {
            job_id,
            limit,
            json,
        } => {
            let wp = wal_path();
            if !wp.exists() {
                if json {
                    println!("{{\"events\": [], \"total\": 0}}");
                } else {
                    println!("no WAL file");
                }
                return Ok(());
            }
            let reader = WalReader::load(&wp)?;
            let filtered: Vec<_> = match &job_id {
                Some(id) => reader.events().iter().filter(|e| e.job_id == *id).collect(),
                None => reader.events().iter().collect(),
            };
            let show: Vec<_> = filtered.iter().rev().take(limit).rev().collect();

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "events": show,
                        "total": filtered.len(),
                        "showing": show.len(),
                    }))?
                );
            } else {
                println!("{} events:", show.len());
                for e in show.iter().rev() {
                    println!(
                        "  {:?}  job={}  cap={}",
                        e.event_type,
                        e.job_id,
                        e.capability.as_deref().unwrap_or("-")
                    );
                    if let Some(ref tel) = e.telemetry_before {
                        println!(
                            "    cpu={}  ram={}  procs={}",
                            tel.system.cpu_model,
                            tel.system.ram_free,
                            e.process_before.as_ref().map(|p| p.total_processes).unwrap_or(0)
                        );
                    }
                    if let Some(ref tel) = e.telemetry_after {
                        println!(
                            "    after: ram={}  procs={}",
                            tel.system.ram_free,
                            e.process_after.as_ref().map(|p| p.total_processes).unwrap_or(0)
                        );
                    }
                    if let Some(ref out) = e.output {
                        println!("    {}", out);
                    }
                    if let Some(ref err) = e.error {
                        println!("    err: {}", err);
                    }
                }
            }
            Ok(())
        }

        Commands::Undo { job_id, dry_run } => {
            let wp = wal_path();
            if !wp.exists() {
                return Err("no WAL file".into());
            }
            let reader = WalReader::load(&wp)?;
            let events: Vec<_> = reader
                .events()
                .iter()
                .filter(|e| e.job_id == job_id)
                .collect();
            if events.is_empty() {
                return Err(format!("no events for job {}", job_id).into());
            }

            let bd = backup_dir().join(&job_id);
            if !bd.exists() {
                return Err(format!("no backup for job {}", job_id).into());
            }

            let mut target_paths: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for event in &events {
                if let Some(output) = &event.output {
                    let path = output.get("path").and_then(|p| p.as_str()).or_else(|| {
                        output
                            .get("data")
                            .and_then(|d| d.get("path"))
                            .and_then(|p| p.as_str())
                    });
                    let backup = output
                        .get("data")
                        .and_then(|d| d.get("backup_path"))
                        .and_then(|b| b.as_str());
                    if let (Some(p), Some(b)) = (path, backup) {
                        if let Some(filename) =
                            std::path::Path::new(b).file_name().and_then(|n| n.to_str())
                        {
                            target_paths.insert(filename.to_string(), p.to_string());
                        }
                    }
                }
            }

            if dry_run {
                println!(
                    "Would restore {} file(s) for job {}:",
                    bd.read_dir()?.count(),
                    job_id
                );
                for entry in std::fs::read_dir(&bd)? {
                    let entry = entry?;
                    let bp = entry.path();
                    if bp.is_file() {
                        if let Some(target) = target_paths.get(&job_id) {
                            println!("  {} → {}", bp.display(), target);
                        } else {
                            println!("  {} (unknown target)", bp.display());
                        }
                    }
                }
                return Ok(());
            }

            let mut restored = 0;
            for entry in std::fs::read_dir(&bd)? {
                let entry = entry?;
                let bp = entry.path();
                if bp.is_file() {
                    let filename = bp.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    let target = if let Some(target_path) = target_paths.get(filename) {
                        std::path::PathBuf::from(target_path)
                    } else {
                        return Err(format!(
                            "Cannot determine original path for backup file {:?}. \
                             WAL does not contain the target path for job {}.",
                            bp.file_name().unwrap_or_default(),
                            job_id
                        )
                        .into());
                    };
                    BackupManager::new(backup_dir())?.restore(&bp, &target)?;
                    restored += 1;
                }
            }
            println!("restored {} file(s) for job {}", restored, job_id);
            Ok(())
        }

        Commands::Telemetry { json } => {
            let tel = Telemetry::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&tel)?);
            } else {
                tel.print_report();
            }
            Ok(())
        }

        Commands::Processes { json } => {
            let snap = ProcessSnapshot::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                snap.print_report();
            }
            Ok(())
        }

        Commands::Config { action } => match action {
            ConfigAction::AllowedPaths { subaction } => match subaction {
                AllowedPathsAction::Add { paths } => {
                    let mut config = RuntimoConfig::load();
                    for p in &paths {
                        if !config.allowed_paths.contains(p) {
                            config.allowed_paths.push(p.clone());
                        }
                    }
                    config.save().map_err(|e| format!("config save failed: {}", e))?;
                    println!(
                        "added {} path(s) to {}",
                        paths.len(),
                        RuntimoConfig::config_path().display()
                    );
                    Ok(())
                }
                AllowedPathsAction::Remove { paths } => {
                    let mut config = RuntimoConfig::load();
                    config.allowed_paths.retain(|p| !paths.contains(p));
                    config.save().map_err(|e| format!("config save failed: {}", e))?;
                    println!(
                        "removed {} path(s) from {}",
                        paths.len(),
                        RuntimoConfig::config_path().display()
                    );
                    Ok(())
                }
                AllowedPathsAction::List => {
                    let config = RuntimoConfig::load();
                    let all = RuntimoConfig::get_allowed_prefixes();
                    println!("configured paths:");
                    for p in &config.allowed_paths {
                        println!("  {}", p);
                    }
                    println!("effective paths (defaults + env + config):");
                    for p in &all {
                        println!("  {}", p);
                    }
                    Ok(())
                }
            },
        },
    }
}
