//! Example 3: Write, Overwrite, and Undo (restore from backup)
//!
//! Demonstrates FileWrite's built-in backup-before-mutate mechanism:
//!   1. Write a new file
//!   2. Read it back to confirm
//!   3. Overwrite it (automatically creates backup)
//!   4. Restore the original from backup
//!
//! Run: cargo run --example write_and_undo -p runtimo-core

use runtimo_core::{execute_with_telemetry, BackupManager, FileRead, FileWrite};
use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    let tmp_path = "/tmp/runtimo_example_undo.txt";
    let wal_path = Path::new("/tmp/runtimo_example_undo.wal");
    let backup_dir = PathBuf::from("/tmp/runtimo_example_backups");

    // Cleanup from any previous run
    let _ = std::fs::remove_file(tmp_path);
    let _ = std::fs::remove_file(wal_path);
    let _ = std::fs::remove_dir_all(&backup_dir);

    // ── Step 1: Write initial content ──────────────────────────────
    println!("=== Step 1: Write initial file ===");
    let file_write = FileWrite::new(backup_dir.clone());
    let result = execute_with_telemetry(
        &file_write,
        &serde_json::json!({
            "path": tmp_path,
            "content": "Version 1 - Original content\n"
        }),
        false,
        wal_path,
    )?;
    println!(
        "Write result: success={}, message={:?}",
        result.success, result.output.message
    );

    // Verify with FileRead
    let read_result = execute_with_telemetry(
        &FileRead,
        &serde_json::json!({ "path": tmp_path }),
        false,
        wal_path,
    )?;
    if let Some(content) = read_result
        .output
        .data
        .get("content")
        .and_then(|v| v.as_str())
    {
        println!("File content: {content:?}");
    }

    // ── Step 2: Overwrite (creates backup automatically) ───────────
    println!("\n=== Step 2: Overwrite file (backup created) ===");
    let file_write2 = FileWrite::new(backup_dir.clone());
    let result2 = execute_with_telemetry(
        &file_write2,
        &serde_json::json!({
            "path": tmp_path,
            "content": "Version 2 - Overwritten content\n"
        }),
        false,
        wal_path,
    )?;
    println!(
        "Overwrite result: success={}, message={:?}",
        result2.success, result2.output.message
    );

    // Show backup path from the output data
    if let Some(backup_path) = result2
        .output
        .data
        .get("backup_path")
        .and_then(|v| v.as_str())
    {
        println!("Backup created at: {backup_path}");
    }

    // Verify overwritten content
    let read_result2 = execute_with_telemetry(
        &FileRead,
        &serde_json::json!({ "path": tmp_path }),
        false,
        wal_path,
    )?;
    if let Some(content) = read_result2
        .output
        .data
        .get("content")
        .and_then(|v| v.as_str())
    {
        println!("File content after overwrite: {content:?}");
    }

    // ── Step 3: Restore from backup ────────────────────────────────
    println!("\n=== Step 3: Restore from backup ===");
    let backup_path_str = result2
        .output
        .data
        .get("backup_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No backup path in write output"))?;

    let backup_mgr = BackupManager::new(backup_dir.clone());
    backup_mgr.restore(Path::new(backup_path_str), Path::new(tmp_path))?;
    println!("Restored from: {backup_path_str}");

    // Verify restored content
    let read_result3 = execute_with_telemetry(
        &FileRead,
        &serde_json::json!({ "path": tmp_path }),
        false,
        wal_path,
    )?;
    if let Some(content) = read_result3
        .output
        .data
        .get("content")
        .and_then(|v| v.as_str())
    {
        println!("File content after restore: {content:?}");
    }

    // Cleanup
    let _ = std::fs::remove_file(tmp_path);
    let _ = std::fs::remove_file(wal_path);
    let _ = std::fs::remove_dir_all(&backup_dir);

    println!("\n=== Done — temp files cleaned up ===");
    Ok(())
}
