//! Example 2: Telemetry & Process Snapshot Demo
//!
//! Captures hardware telemetry and process snapshots, then prints
//! human-readable reports.
//!
//! Run: cargo run --example telemetry_demo -p runtimo-core

use runtimo_core::{ProcessSnapshot, Telemetry};

fn main() {
    println!("Capturing hardware telemetry...");
    let telemetry = Telemetry::capture();
    telemetry.print_report();

    println!("\nCapturing process snapshot...");
    let snapshot = ProcessSnapshot::capture();
    snapshot.print_report();

    // Also demonstrate programmatic access
    println!("\n--- Programmatic Summary ---");
    println!("CPU model     : {}", telemetry.system.cpu_model);
    println!("RAM free      : {}", telemetry.system.ram_free);
    println!("Disk free     : {}", telemetry.system.disk_free);
    println!("GPU devices   : {}", telemetry.hardware.gpu_devices);
    println!("TPU devices   : {}", telemetry.hardware.tpu_devices);
    println!("Accelerators  : {:#?}", telemetry.hardware.accelerators);
    println!("Total procs   : {}", snapshot.summary.total_processes);
    println!("Total CPU %   : {:.1}", snapshot.summary.total_cpu_percent);
    println!("Zombies       : {}", snapshot.summary.zombie_count);

    if let Some(ref top) = snapshot.summary.top_cpu_consumer {
        println!("Top CPU proc  : {top}");
    }
    if let Some(ref top) = snapshot.summary.top_mem_consumer {
        println!("Top MEM proc  : {top}");
    }
}
