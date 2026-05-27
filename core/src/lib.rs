//! Runtimo Core — Agent-centric capability runtime.
//!
//! Runtimo provides structured execution, resource limits, crash recovery,
//! and two-layer telemetry (hardware + process tracking) for machines that
//! cannot be factory-reset. Every capability execution captures before/after
//! snapshots, enabling full audit trails and undo support.
//!
//! # Architecture
//!
//! - **Capabilities** — Pluggable operations implementing the [`Capability`] trait
//! - **Jobs** — Lifecycle-tracked execution units ([`Job`], [`JobState`])
//! - **Telemetry** — Hardware awareness ([`Telemetry`])
//! - **Process Snapshot** — Running process awareness ([`ProcessSnapshot`])
//! - **WAL** — Append-only crash recovery log ([`WalWriter`]/[`WalReader`])
//! - **Backup** — Undo support via pre-mutation file backups ([`BackupManager`])
//! - **Resource Guards** — Circuit breaker via [`LlmoSafeGuard`]
//!
//! # Quick Start
//!
//! ```rust
//! use runtimo_core::{FileRead, Capability, Context};
//! use serde_json::json;
//!
//! let cap = FileRead;
//! assert_eq!(cap.name(), "FileRead");
//! ```
//!
//! # Execution with Full Telemetry
//!
//! ```rust,ignore
//! use runtimo_core::{FileRead, execute_with_telemetry};
//! use serde_json::json;
//! use std::path::Path;
//!
//! let cap = FileRead;
//! let result = execute_with_telemetry(
//!     &cap,
//!     &json!({"path": "/tmp/test.txt"}),
//!     false,
//!     Path::new("/tmp/runtimo.wal"),
//! ).unwrap();
//! assert!(result.success);
//! ```
//!
//! # Performance (Measured on AMD EPYC 7B13)
//!
//! | Operation | Latency | Notes |
//! |-----------|---------|-------|
//! | Cold start | <1s | Binary load + init |
//! | FileRead | <10ms | Small files (<1KB) |
//! | FileWrite | <50ms | Includes backup copy |
//! | Telemetry capture | <100ms | 15+ shell subprocesses |
//! | Process snapshot | <50ms | ps aux parse |
//! | Memory baseline | <50MB | RSS at idle |
//!
//! # Feature Flags
//!
//! No optional features currently. All functionality is included by default.

pub mod backup;
pub mod capabilities;
pub mod capability;
pub mod cmd;
pub mod config;
pub mod executor;
pub mod job;
pub mod llmosafe;
pub mod monitor;
pub mod processes;
pub mod schema;
pub mod session;
pub mod telemetry;
pub mod validation;
pub mod wal;

pub use backup::BackupManager;
pub use capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo};
pub use capability::{Capability, CapabilityRegistry, Context, Output};
pub use config::RuntimoConfig;
pub use executor::{execute_with_telemetry, execute_with_telemetry_and_session, ExecutionResult};
pub use job::{Job, JobId, JobState};
pub use llmosafe::LlmoSafeGuard;
pub use monitor::{HealthAlert, HealthMonitor, HealthState};
pub use processes::ProcessSnapshot;
pub use session::{Session, SessionManager};
pub use telemetry::Telemetry;
pub use wal::{WalEvent, WalEventType, WalReader, WalWriter};

/// Error types for runtimo-core.
///
/// Covers all failure modes: state transitions, schema validation,
/// capability execution, WAL/backup errors, resource limits, and telemetry.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid job state transition attempted.
    #[error("Invalid job state transition: {from:?} -> {to:?}")]
    InvalidTransition { from: JobState, to: JobState },

    /// JSON schema validation failed for capability arguments.
    #[error("Schema validation failed: {0}")]
    SchemaValidationFailed(String),

    /// Requested capability not found in registry.
    #[error("Capability not found: {0}")]
    CapabilityNotFound(String),

    /// Capability execution failed.
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    /// Write-Ahead Log operation failed.
    #[error("WAL error: {0}")]
    WalError(String),

    /// Backup/restore operation failed.
    #[error("Backup error: {0}")]
    BackupError(String),

    /// System resource limit exceeded (CPU, RAM, or zombie count).
    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    /// Telemetry capture failed.
    #[error("Telemetry error: {0}")]
    TelemetryError(String),
}

/// Result alias for runtimo-core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Utility functions for path management.
pub mod utils {
    use std::path::PathBuf;

    /// Returns the data directory following XDG spec.
    pub fn data_dir() -> PathBuf {
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

    /// Returns the WAL path (env override or default).
    pub fn wal_path() -> PathBuf {
        std::env::var("RUNTIMO_WAL_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir().join("wal.jsonl"))
    }

    /// Returns the backup directory (env override or default).
    pub fn backup_dir() -> PathBuf {
        std::env::var("RUNTIMO_BACKUP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir().join("backups"))
    }

    /// Generates a unique ID from 16 random bytes (32 hex chars).
    ///
    /// Uses `/dev/urandom` for collision resistance — P(collision) < 10⁻¹⁵
    /// even at 100 IDs/sec for 1 hour. Falls back to timestamp if urandom
    /// is unavailable (e.g., non-Linux platforms).
    pub fn generate_id() -> String {
        let mut bytes = [0u8; 16];
        if std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes).ok())
            .is_some()
        {
            bytes.iter().map(|b| format!("{:02x}", b)).collect()
        } else {
            // Fallback: timestamp-based (collision possible but rare)
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("{:x}", ts)
        }
    }
}
