//! moe CLI

use clap::{Parser, Subcommand};
use runtimo_core::{
    capabilities::{FileRead, FileWrite},
    execute_with_telemetry, BackupManager, CapabilityRegistry, ProcessSnapshot, Telemetry,
    WalReader,
};
use std::error::Error;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "moe", about = "Runtimo CLI - Agent capability runtime")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a capability
    Run {
        #[arg(short, long)]
        capability: String,
        #[arg(short, long, default_value = "{}")]
        args: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Check job status
    Status {
        #[arg(short, long)]
        job_id: Option<String>,
    },
    /// Undo a completed job
    Undo {
        #[arg(short, long)]
        job_id: String,
    },
    /// View WAL logs
    Logs {
        #[arg(short, long)]
        job_id: Option<String>,
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
    /// Print system telemetry
    Telemetry,
    /// Print process snapshot
    Processes,
    /// List capabilities
    List,
}

// WAL and backup paths use XDG_DATA_HOME for security (avoid world-writable /tmp).
// Falls back to ~/.local/share/runtimo/, then /tmp/runtimo/ if HOME is unavailable.
// Override with env vars for custom deployments.

fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("runtimo")
}

fn wal_path() -> PathBuf {
    std::env::var("RUNTIMO_WAL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("wal.jsonl"))
}

fn backup_dir() -> PathBuf {
    std::env::var("RUNTIMO_BACKUP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("backups"))
}

fn make_registry() -> CapabilityRegistry {
    let mut reg = CapabilityRegistry::new();
    reg.register(FileRead);
    reg.register(FileWrite::new(backup_dir()));
    reg
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            capability,
            args,
            dry_run,
        } => {
            let args: serde_json::Value =
                serde_json::from_str(&args).map_err(|e| format!("invalid JSON args: {}", e))?;

            let reg = make_registry();
            let cap = reg
                .get(&capability)
                .ok_or_else(|| format!("capability not found: {}", capability))?;

            let wp = wal_path();
            if let Some(parent) = wp.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let result = execute_with_telemetry(cap, &args, dry_run, &wp)?;

            println!(
                "job: {}  cap: {}  ok: {}",
                result.job_id, result.capability, result.success
            );
            if let Some(msg) = &result.output.message {
                println!("  {}", msg);
            }
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
            println!("  {}", serde_json::to_string_pretty(&result.output.data)?);
            Ok(())
        }

        Commands::Status { job_id } => {
            let wp = wal_path();
            if !wp.exists() {
                println!("no jobs yet");
                return Ok(());
            }
            let reader = WalReader::load(&wp)?;
            match job_id {
                Some(id) => {
                    let events: Vec<_> =
                        reader.events().iter().filter(|e| e.job_id == id).collect();
                    if events.is_empty() {
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
                    println!("{} events total:", events.len());
                    let mut jobs: std::collections::HashMap<&str, Vec<&runtimo_core::WalEvent>> =
                        std::collections::HashMap::new();
                    for e in events {
                        jobs.entry(&e.job_id).or_default().push(e);
                    }
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
            Ok(())
        }

    Commands::Undo { job_id } => {
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

        // Extract original target paths from WAL events
        let mut target_paths: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for event in &events {
            if let Some(output) = &event.output {
                if let Some(path) = output.get("path").and_then(|p| p.as_str()) {
                    target_paths.insert(event.job_id.clone(), path.to_string());
                }
            }
        }

        let mut restored = 0;
        for entry in std::fs::read_dir(&bd)? {
            let entry = entry?;
            let bp = entry.path();
            if bp.is_file() {
                // Get original path from WAL, or fall back to filename-based reconstruction
                let target = if let Some(target_path) = target_paths.get(&job_id) {
                    std::path::PathBuf::from(target_path)
                } else {
                    bd.parent()
                        .ok_or_else(|| "Invalid backup structure".to_string())?
                        .parent()
                        .ok_or_else(|| "Invalid backup structure".to_string())?
                        .join(bp.file_name().ok_or_else(|| "Invalid filename".to_string())?)
                };
                BackupManager::new(backup_dir()).restore(&bp, &target)?;
                restored += 1;
            }
        }
        println!("restored {} file(s) for job {}", restored, job_id);
        Ok(())
    }

        Commands::Logs { job_id, limit } => {
            let wp = wal_path();
            if !wp.exists() {
                println!("no WAL file");
                return Ok(());
            }
            let reader = WalReader::load(&wp)?;
            let filtered: Vec<_> = match &job_id {
                Some(id) => reader.events().iter().filter(|e| e.job_id == *id).collect(),
                None => reader.events().iter().collect(),
            };
            let show: Vec<_> = filtered.iter().rev().take(limit).collect();

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
            Ok(())
        }

        Commands::Telemetry => {
            Telemetry::capture().print_report();
            Ok(())
        }

        Commands::Processes => {
            ProcessSnapshot::capture().print_report();
            Ok(())
        }

        Commands::List => {
            let reg = make_registry();
            let caps = reg.list();
            println!("{} capabilities:", caps.len());
            for c in caps {
                println!("  {}", c);
            }
            Ok(())
        }
    }
}
