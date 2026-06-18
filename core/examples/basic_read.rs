//! Example 1: Basic FileRead
//!
//! Creates a temp file, reads it with the FileRead capability via
//! execute_with_telemetry, and prints the content.
//!
//! Run: cargo run --example basic_read -p runtimo-core

use runtimo_core::{execute_with_telemetry, FileRead};
use std::fs;
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let tmp_path = "/tmp/runtimo_example_basic_read.txt";
    let wal_path = Path::new("/tmp/runtimo_example_basic.wal");

    // Cleanup from any previous run
    let _ = fs::remove_file(tmp_path);
    let _ = fs::remove_file(wal_path);

    // Create a temp file
    let test_content = "Hello from Runtimo Core!\n";
    fs::write(tmp_path, test_content)?;
    println!("Created temp file: {tmp_path}");

    // Execute FileRead through the telemetry-wrapped executor
    let result = execute_with_telemetry(
        &FileRead,
        &serde_json::json!({ "path": tmp_path }),
        false,
        wal_path,
    )?;

    println!("\n--- Execution Result ---");
    println!("Job ID     : {}", result.job_id);
    println!("Capability : {}", result.capability);
    println!("Success    : {}", result.success);
    println!("WAL seq    : {}", result.wal_seq);

    if let Some(content) = result.output.data.as_ref().and_then(|d| d.get("content")).and_then(|v| v.as_str()) {
        println!("\n--- File Content ---");
        print!("{content}");
    }

    println!("\nMessage    : {}", result.output.output);

    println!(
        "\nProcesses before: {} | after: {}",
        result.process_before.total_processes, result.process_after.total_processes
    );

    // Cleanup
    let _ = fs::remove_file(tmp_path);
    let _ = fs::remove_file(wal_path);

    Ok(())
}
