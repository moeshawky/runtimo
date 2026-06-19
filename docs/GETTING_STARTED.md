# Getting Started with Runtimo

**Version:** 0.7.1
**Last Updated:** 2026-06-19

This guide walks you through using Runtimo for the first time. By the end, you'll have executed capabilities with full telemetry, process tracking, and crash recovery.

## Prerequisites

- Rust 1.70+ (for edition 2021)
- Linux or macOS (for `/proc` telemetry)
- `cargo` installed

## Installation

### From Source

```bash
git clone https://github.com/your-org/runtimo.git
cd runtimo
cargo build --release
```

The release binary is at `./target/release/runtimo`. For debug builds, use `cargo build`
and find the binary at `./target/release/runtimo`.

### From Cargo

```bash
# Add to your Cargo.toml
[dependencies]
runtimo-core = "0.7"
```

## Quick Start (5 Minutes)

### Step 1: Build the CLI

```bash
cd runtimo
cargo build
```

**Expected output:**
```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.2s
```

### Step 2: List Available Capabilities

```bash
./target/release/runtimo list
```

**Expected output:**
```
      FileRead  read file. path validated. no dirs, no traversal.
     ShellExec  execute shell command via sh -c with timeout, audit trail, detokenized blocklist, path restrictions, env sanitization, and PID tracking. blocks: rm, shred, mkfs, fdisk, dd, shutdown, chown, chmod, kill, mount, iptables, interpreters (opt-in), network tools (opt-in), fork bombs, env dumpers.
       GitExec  git operations: clone, pull, commit, revert, clean, status. state tracking (sha, branch, remote), SSRF-blocked URLs, secret detection, timeout, undo via backup.
          Kill  terminate process by PID with PID reuse protection. protected: init (1), kthreadd (2), self, parent, session/group leaders, systemd services. signals: 1-31, 64 (SIGRTMIN).
          Undo  restore from backup. use `runtimo logs` for job IDs.
     FileWrite  write file. auto-backup for undo. append ok.
```

### Step 3: View System Telemetry

```bash
./target/release/runtimo telemetry
```

**Expected output:**
```
============================================================ RUNTIMO TELEMETRY [1778925313] ============================================================
--- SYSTEM ---
CPU : AMD EPYC 7B13
RAM : 30Gi total, 4.7Gi free
Disk : 148G total, 40G free (73% used)
Uptime: up 1 day, 2 hours
Load : 0.67, 0.47, 0.49

--- HARDWARE ---
TPU Devices: 0
GPU Devices: 0
JAX: Not available

--- SERVICES ---
vLLM: not installed
Port 8200: NOT BOUND

--- NETWORK ---
Public IP: 34.45.218.104
Tunnel: running
```

**What this measures:**
- CPU model, RAM free/total, disk usage
- TPU/GPU device detection
- Service availability (vLLM, port bindings)
- Network status (public IP, tunnel)

### Step 4: View Process Snapshot

```bash
./target/release/runtimo processes
```

**Expected output:**
```
================================================================================
PROCESS SNAPSHOT [1778925297]
================================================================================
--- SUMMARY ---
Total Processes: 185
Total CPU: 26.7%
Total Memory: 7.2%
Zombies: 0
Top CPU: python3 (7.6%)
Top Memory: python3 (1.4%)

--- TOP 10 BY CPU ---
1. 80605  moeshaw+  7.6  1.4  73445.4G  453.7G  "Sl+"  python3
2. 194444  moeshaw+  7.6  2.0  73908.8G  625.9G  "Sl+"  python3
...
```

**What this measures:**
- Total process count
- Total CPU/memory usage across all processes
- Zombie process count (should be 0)
- Top consumers by CPU and memory
- Parent PID (PPID) tracking for process lineage

### Step 5: Read a File

```bash
./target/release/runtimo run -c FileRead -a '{"path":"/etc/hostname"}'
```

**Expected output:**
```json
{
  "success": true,
  "data": {
    "content": "my-hostname\n"
  },
  "telemetry_before": { ... },
  "telemetry_after": { ... },
  "process_before": { ... },
  "process_after": { ... }
}
```

**What happened:**
1. Hardware telemetry captured (CPU, RAM, disk, services, network)
2. Process snapshot captured (185 processes, 0 zombies)
3. Resource guard checked (CPU < 90%, RAM < 90%)
4. Capability validated (path exists, no traversal)
5. File read
6. After telemetry captured
7. WAL event logged (fsync'd)

### Step 6: Write a File

```bash
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/hello.txt","content":"hello runtimo"}'
```

**Expected output:**
```json
{
  "success": true,
  "data": {
    "path": "/tmp/hello.txt",
    "bytes_written": 13
  },
  ...
}
```

**What happened:**
1. Backup created (if file existed)
2. Content written to `/tmp/hello.txt`
3. WAL event logged

### Step 6b: Execute a Shell Command

```bash
./target/release/runtimo run -c ShellExec -a '{"cmd":"echo hello && whoami"}'
```

**Expected output:**
```json
{
  "success": true,
  "data": {
    "stdout": "hello\nuser\n",
    "stderr": "",
    "exit_code": 0
  },
  ...
}
```

**What happened:**
1. Command validated against dangerous blocklist (safe: passes)
2. Process group created for isolation
3. Command executed via `sh -c` (supports pipes, chains, vars)
4. Stdout/stderr captured (10MB limit)
5. WAL event logged — in debug builds, CommandExecuted event records full output

### Step 7: Verify the File

```bash
./target/release/runtimo run -c FileRead -a '{"path":"/tmp/hello.txt"}'
```

**Expected output:**
```json
{
  "success": true,
  "data": {
    "content": "hello runtimo"
  },
  ...
}
```

### Step 8: Dry Run (Validate Without Executing)

```bash
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"test"}' --dry-run
```

**Expected output:**
```json
{
  "success": true,
  "data": {
    "dry_run": true,
    "path": "/tmp/test.txt"
  },
  ...
}
```

**What happened:**
- File was NOT written
- Backup was NOT created
- Validation passed
- WAL event logged as dry-run

### Step 9: View WAL Logs

```bash
./target/release/runtimo logs
```

**Expected output:**
```
[1] JobStarted: FileRead
[2] JobCompleted: FileRead
[3] JobStarted: FileWrite
[4] JobCompleted: FileWrite
[5] JobStarted: ShellExec
[6] JobCompleted: ShellExec
...
```

> **Note:** In debug builds, `CommandExecuted` events appear after each ShellExec completion,
> recording the command string, truncated stdout/stderr, and exit code for error-pattern analysis.

### Step 10: Undo a FileWrite

```bash
# First, overwrite a file
./target/release/runtimo run -c FileWrite -a '{"path":"/tmp/test.txt","content":"new content"}'

# Then undo it
./target/release/runtimo undo -j <job_id_from_logs>
```

**Expected output:**
```
Restored 1 file(s) from job <job_id>
```

### Step 11: Dispatch a Background Job (new in 0.7.1)

```bash
# Start the daemon (auto-starts on first dispatch)
runtimo dispatch -c ShellExec -a '{"cmd":"sleep 10"}'

# Check job status
runtimo status -j <job_id_from_output>

# Wait for it to complete
runtimo wait -j <job_id>
```

**What happened:**
1. CLI validates the command against the blocklist (same as `run`)
2. Job serialized and sent to daemon via JSON-RPC
3. Daemon executes in background
4. Status tracked via WAL events
5. `runtimo wait` polls until completion

### Security: What Gets Blocked

Runtimo blocks dangerous operations by default:

| Category | What's blocked | How to bypass |
|----------|---------------|---------------|
| **File destruction** | `rm`, `shred`, `dd` | Use `FileWrite` + `Undo` capabilities |
| **System takeover** | `chown`, `chmod`, `chgrp` | — blocked permanently |
| **Kernel/filesystem** | `mkfs`, `fdisk`, `mount`, `iptables` | — blocked permanently |
| **Power state** | `shutdown`, `reboot`, `halt`, `poweroff` | — blocked permanently |
| **Credential files** | `.env`, `.env.*` via FileWrite | Use alternative filename |
| **Path escape** | `..`, `~`, `$HOME`, null bytes | Use full paths within allowed dirs |
| **Network tools** | `curl`, `wget`, `ssh`, `nc`, etc. | Set `RUNTIMO_ENABLE_NETWORK=1` |

### Quoting Bypass Protection (new in 0.7.1)

ShellExec normalizes shell quoting AND backslash escapes before applying the blocklist:

```bash
# All of these are caught — no execution happens
runtimo run -c ShellExec -a '{"cmd":"r\"m\" -rf /"}'   # shell quoting
runtimo run -c ShellExec -a '{"cmd":"r\\m -rf /"}'      # backslash
runtimo run -c ShellExec -a "{\"cmd\":\"rm --recursive /\"}"  # long option
# → blocked: dangerous command blocked: rm command blocked
```

## Using as a Library

### Basic File Read

```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

fn main() -> runtimo_core::Result<()> {
    let cap = FileRead;
    let args = json!({"path": "/etc/hostname"});
    let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
    
    println!("Success: {}", result.success);
    println!("Content: {:?}", result.output.data.get("content"));
    
    Ok(())
}
```

### Basic File Write

```rust
use runtimo_core::{FileWrite, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

fn main() -> runtimo_core::Result<()> {
    let cap = FileWrite::new()?;  // backup_dir derived from data_dir() automatically
    let args = json!({
        "path": "/tmp/hello.txt",
        "content": "hello from runtimo"
    });
    let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
    
    println!("Written: {:?}", result.output.data.get("path"));
    
    Ok(())
}
```

### With Custom Context

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

fn main() -> runtimo_core::Result<()> {
    let cap = FileRead;
    let args = json!({"path": "/etc/hostname"});
    
    // Custom context (e.g., for job tracking)
    let ctx = Context {
        dry_run: false,
        job_id: "my-job-123".to_string(),
        working_dir: std::env::current_dir()?,
    };
    
    let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
    
    Ok(())
}
```

## Understanding the Output

### ExecutionResult Structure

```rust
pub struct ExecutionResult {
    pub job_id: String,              // Unique job identifier
    pub capability: String,          // Capability name (e.g., "FileRead")
    pub success: bool,               // true if execution succeeded
    pub output: Output,              // Capability-specific output
    pub telemetry_before: Telemetry, // Hardware state before
    pub telemetry_after: Telemetry,  // Hardware state after
    pub process_before: ProcessSummary, // Process state before
    pub process_after: ProcessSummary,  // Process state after
    pub wal_seq: u64,                // WAL sequence number
}
```

### Telemetry Structure

```rust
pub struct Telemetry {
    pub timestamp: i64,              // Unix timestamp
    pub cpu_model: String,           // e.g., "AMD EPYC 7B13"
    pub ram_total_gb: f64,           // Total RAM in GB
    pub ram_free_gb: f64,            // Free RAM in GB
    pub disk_total_gb: f64,          // Total disk in GB
    pub disk_free_gb: f64,           // Free disk in GB
    pub disk_used_percent: f64,      // Disk usage percentage
    pub uptime_secs: i64,            // System uptime in seconds
    pub load_avg: [f64; 3],          // Load average (1m, 5m, 15m)
    pub tpu_count: u32,              // TPU devices
    pub gpu_count: u32,              // GPU devices
    pub jax_available: bool,         // JAX available?
    pub vllm_version: Option<String>, // vLLM version
    pub vllm_running: bool,          // vLLM running?
    pub port_8200_bound: bool,       // Port 8200 bound?
    pub public_ip: Option<String>,   // Public IP
    pub tunnel_running: bool,        // Cloudflared tunnel running?
}
```

### ProcessSummary Structure

```rust
pub struct ProcessSummary {
    pub total_processes: u32,        // Total process count
    pub total_cpu_percent: f64,      // Total CPU usage
    pub total_mem_percent: f64,      // Total memory usage
    pub zombie_count: u32,           // Zombie process count
    pub top_cpu_process: String,     // Top CPU consumer name
    pub top_mem_process: String,     // Top memory consumer name
}
```

## Common Patterns

> **Note:** All patterns assume `let fw = FileWrite::new()?;` is called once at the top of the
> function. `FileRead` still works as a unit struct (`&FileRead`).

### Pattern 1: Read-Modify-Write

```rust
let fw = FileWrite::new()?;

// Read existing content
let read_args = json!({"path": "/tmp/config.txt"});
let read_result = execute_with_telemetry(&FileRead, &read_args, false, wal_path)?;

// Modify content
let mut content = read_result.output.data["content"].as_str().unwrap().to_string();
content.push_str("\n# New line");

// Write back
let write_args = json!({
    "path": "/tmp/config.txt",
    "content": content
});
let write_result = execute_with_telemetry(&fw, &write_args, false, wal_path)?;
```

### Pattern 2: Conditional Write

```rust
let fw = FileWrite::new()?;

// Check if file exists first
let check_args = json!({"path": "/tmp/important.txt"});
match execute_with_telemetry(&FileRead, &check_args, false, wal_path) {
    Ok(_) => {
        // File exists, skip write
        println!("File exists, skipping write");
    }
    Err(_) => {
        // File doesn't exist, create it
        let write_args = json!({
            "path": "/tmp/important.txt",
            "content": "created by runtimo"
        });
        execute_with_telemetry(&fw, &write_args, false, wal_path)?;
    }
}
```

### Pattern 3: Dry Run Before Execution

```rust
let fw = FileWrite::new()?;
let args = json!({"path": "/tmp/test.txt", "content": "test"});

// First, validate with dry run
let dry_result = execute_with_telemetry(&fw, &args, true, wal_path)?;
assert!(dry_result.success);

// Then execute for real
let real_result = execute_with_telemetry(&fw, &args, false, wal_path)?;
```

## Troubleshooting

### "Resource limit exceeded"

**Symptom:** `Error: Resource limit exceeded: CPU > 90%`

**Cause:** System is under heavy load

**Fix:**
1. Wait for load to decrease
2. Close other applications
3. Check `runtimo processes` for runaway processes

### "Path traversal detected"

**Symptom:** `Error: Schema validation failed: Path traversal detected`

**Cause:** Path contains `..` sequence

**Fix:** Use absolute paths without traversal sequences

### "Capability not found"

**Symptom:** `Error: Capability not found: FileRead`

**Cause:** Capability not registered

**Fix:** Ensure capability is added to the registry

### "WAL write failed"

**Symptom:** `Error: WAL error: Permission denied`

**Cause:** WAL path is not writable

**Fix:** Set `RUNTIMO_WAL_PATH` to a writable directory

## Next Steps

1. **Read the API documentation** - See `docs/API.md`
2. **Explore the architecture** - See `docs/ARCHITECTURE.md`
3. **Check runbooks** - See `docs/runbooks/`
4. **Review examples** - See `core/examples/`

## Getting Help

- **CLI help:** `./target/release/runtimo --help` (compiler-error style: `req=` required, `opt=` optional, `blk=` blocked, `ex=` example)
- **Documentation:** `docs/` directory
- **Examples:** `core/examples/` directory
