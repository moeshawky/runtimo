Runtimo Core - Agent-centric capability runtime for persistent machines
================================================================================

**Runtimo Core** is a Rust library that provides a capability execution engine 
designed for machines that cannot be factory-reset. Every capability execution 
is wrapped with telemetry, resource guards, and crash recovery.

# Overview

Runtimo absorbs agent hallucinations through:
- **Capability validation** - JSON Schema validation before execution
- **Resource guards** - `llmosafe` circuit breaker rejects execution under pressure  
- **Telemetry** - Two-layer awareness (hardware + processes)
- **Crash recovery** - Write-ahead log (WAL) with fsync after each event
- **Backup/undo** - Automatic file backups before mutation

# Architecture

```
┌─────────────────────────────────────────┐
│ Capability Trait                        │
│ - name() -> &str                        │
│ - schema() -> Value                     │
│ - validate(&Value) -> Result<()>        │
│ - execute(&Value, &Context) -> Result<Output> │
└─────────────────────────────────────────┘
                │
                ▼
┌─────────────────────────────────────────┐
│ execute_with_telemetry()                │
│ 1. Telemetry::capture()                 │
│ 2. ProcessSnapshot::capture()           │
│ 3. LlmoSafeGuard::check()               │
│ 4. WalWriter::append(Started)           │
│ 5. capability.validate()                │
│ 6. capability.execute()                 │
│ 7. Telemetry::capture()                 │
│ 8. ProcessSnapshot::capture()           │
│ 9. WalWriter::append(Completed)         │
└─────────────────────────────────────────┘
```

# Getting Started

```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

fn main() -> runtimo_core::Result<()> {
    let cap = FileRead;
    let args = json!({"path": "/etc/hostname"});
    let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
    
    println!("Content: {:?}", result.output.data.get("content"));
    Ok(())
}
```

# Key Concepts

## Capabilities

Capabilities are pluggable operations that implement the [`Capability`] trait:

```rust
pub trait Capability {
    fn name(&self) -> &'static str;
    fn schema(&self) -> Value;
    fn validate(&self, args: &Value) -> Result<()>;
    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output>;
}
```

Built-in capabilities:
- [`FileRead`] - Read file contents with validation
- [`FileWrite`] - Write content to file with backup

## Telemetry

Two-layer telemetry captures hardware and process state:

**Hardware Telemetry** ([`Telemetry`]):
- CPU model, RAM, disk usage
- TPU/GPU detection
- Service availability (vLLM, port bindings)
- Network status (public IP, tunnel)

**Process Snapshot** ([`ProcessSnapshot`]):
- Full process list with PPIDs
- Zombie process detection
- Top CPU/memory consumers

## Resource Guards

[`LlmoSafeGuard`] reads `/proc/stat` and `/proc/self/status`:
- Memory ceiling (80% default)
- CPU load delta measurement
- Pressure score (0-100%)
- Raw entropy score (0-1000)

## Crash Recovery

Write-ahead log ([`WalWriter`], [`WalReader`]) records:
- `JobStarted` - before validation
- `JobCompleted` - after success
- `JobFailed` - on failure

All events are fsync'd for durability.

## Backup/Undo

[`BackupManager`] creates pre-mutation backups:
- Backups stored per job ID
- Undo restores from backup
- Parent directories created automatically

# Examples

## Basic File Read

```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileRead;
let args = json!({"path": "/etc/hostname"});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
```

## Basic File Write

```rust
use runtimo_core::{FileWrite, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::{Path, PathBuf};

let cap = FileWrite::new(PathBuf::from("/tmp/backups")).expect("backup dir");
let args = json!({
    "path": "/tmp/hello.txt",
    "content": "hello runtimo"
});
let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
```

## Dry Run

```rust
use runtimo_core::{FileWrite, Capability, execute_with_telemetry};
use serde_json::json;
use std::path::{Path, PathBuf};

let cap = FileWrite::new(PathBuf::from("/tmp/backups")).expect("backup dir");
let args = json!({"path": "/tmp/test.txt", "content": "test"});

// Validate without executing
let result = execute_with_telemetry(&cap, &args, true, Path::new("/tmp/wal.jsonl"))?;
```

## Custom Context

```rust
use runtimo_core::{FileRead, Capability, Context, execute_with_telemetry};
use serde_json::json;
use std::path::Path;

let cap = FileRead;
let args = json!({"path": "/etc/hostname"});

let ctx = Context {
    dry_run: false,
    job_id: "my-job-123".to_string(),
    working_dir: std::env::current_dir()?,
};

let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl"))?;
```

# Safety

## Path Traversal

All file capabilities reject paths containing `..`:

```rust
// This will fail validation
let args = json!({"path": "../etc/passwd"});
let result = execute_with_telemetry(&FileRead, &args, false, wal_path);
assert!(result.is_err());
```

## Empty Paths

Empty paths are rejected:

```rust
// This will fail validation
let args = json!({"path": ""});
let result = execute_with_telemetry(&FileRead, &args, false, wal_path);
assert!(result.is_err());
```

## Resource Limits

Execution is rejected under pressure:

```rust
// If CPU > 90% or RAM > 90%, execution fails
let result = execute_with_telemetry(&FileRead, &args, false, wal_path);
// May return: Err(ResourceLimitExceeded("CPU > 90%"))
```

# Error Handling

Runtimo uses a custom error type covering all failure modes:

```rust
pub enum Error {
    InvalidTransition { from: JobState, to: JobState },
    SchemaValidationFailed(String),
    CapabilityNotFound(String),
    ExecutionFailed(String),
    WalError(String),
    BackupError(String),
    ResourceLimitExceeded(String),
    TelemetryError(String),
}
```

Handle errors explicitly:

```rust
use runtimo_core::{FileRead, Capability, execute_with_telemetry, Error};
use serde_json::json;
use std::path::Path;

match execute_with_telemetry(&FileRead, &json!({"path": "/test"}), false, Path::new("/tmp/wal.jsonl")) {
    Ok(result) => println!("Success: {:?}", result.output),
    Err(Error::SchemaValidationFailed(msg)) => eprintln!("Validation: {}", msg),
    Err(Error::ExecutionFailed(msg)) => eprintln!("Execution: {}", msg),
    Err(Error::ResourceLimitExceeded(msg)) => eprintln!("Resource: {}", msg),
    Err(e) => eprintln!("Error: {}", e),
}
```

# Testing

```rust
#[cfg(test)]
mod tests {
    use runtimo_core::{FileRead, Capability, execute_with_telemetry};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn test_file_read() {
        let cap = FileRead;
        let args = json!({"path": "/etc/hostname"});
        let result = execute_with_telemetry(&cap, &args, false, Path::new("/tmp/wal.jsonl")).unwrap();
        assert!(result.success);
    }
}
```

# Modules

- [`capability`] - Capability trait and registry
- [`executor`] - Execution pipeline with telemetry
- [`job`] - Job lifecycle management
- [`telemetry`] - Hardware telemetry capture
- [`processes`] - Process snapshot
- [`llmosafe`] - Resource guard
- [`wal`] - Write-ahead log
- [`backup`] - Backup manager
- [`schema`] - JSON Schema validation
- [`capabilities`] - Built-in capabilities

# CLI

The `moe` CLI binary provides:
- `run` - Execute capability
- `list` - List capabilities
- `telemetry` - View hardware state
- `processes` - View process snapshot
- `status` - View job status
- `logs` - View WAL events
- `undo` - Restore from backup

# Version

0.1.0-alpha

# License

Runtimo-1.0
