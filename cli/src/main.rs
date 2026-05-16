//! runtimo CLI — Agent capability runtime

use clap::{Parser, Subcommand};
use runtimo_core::{
    capabilities::{FileRead, FileWrite, Kill, ShellExec, Undo},
    execute_with_telemetry, BackupManager, CapabilityRegistry, ProcessSnapshot, Telemetry,
    WalReader,
};
use std::error::Error;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "runtimo",
    about = "Agent capability runtime with telemetry, process tracking, and crash recovery",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a capability with full telemetry
    Run {
        /// Capability name (e.g., FileRead, FileWrite)
        #[arg(short = 'c', long)]
        capability: String,
        /// JSON arguments for the capability
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
        /// Show capability schema and exit
        #[arg(long)]
        schema: bool,
        /// Execution timeout in seconds (default: 30)
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    /// List available capabilities
    List {
        /// Show schemas for each capability
        #[arg(long)]
        schemas: bool,
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Check job status
    Status {
        /// Filter by job ID
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Output as JSON
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// View WAL logs
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
    /// Undo a completed job (restore from backup)
    Undo {
        /// Job ID to undo
        #[arg(short = 'j', long)]
        job_id: String,
        /// Show what will be restored without executing
        #[arg(long)]
        dry_run: bool,
    },
    /// Print system telemetry
    Telemetry {
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Print process snapshot
    Processes {
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
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
                        "  cpu: {}  ram: {} free  disk: {}%",
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
                                "schema": cap.schema(),
                            }));
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&caps)?);
                }
            } else if schemas {
                for name in caps {
                    if let Some(cap) = reg.get(name) {
                        println!("{}:", name);
                        println!("  {}", serde_json::to_string_pretty(&cap.schema())?);
                    }
                }
            } else {
                println!("{} capabilities:", caps.len());
                for c in caps {
                    println!("  {}", c);
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
                    // Path may be at output.path (FileRead) or output.data.path (FileWrite)
                    let path = output.get("path").and_then(|p| p.as_str()).or_else(|| {
                        output
                            .get("data")
                            .and_then(|d| d.get("path"))
                            .and_then(|p| p.as_str())
                    });
                    if let Some(p) = path {
                        target_paths.insert(event.job_id.clone(), p.to_string());
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
                    let target = if let Some(target_path) = target_paths.get(&job_id) {
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
    }
}
