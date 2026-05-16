//! Runtimo Core — Shared types, traits, and utilities for the Runtimo runtime.
//!
//! Runtimo is an agent-centric capability runtime with hallucination absorption.
//! It provides structured execution, resource limits (via [`LlmoSafeGuard`]),
//! crash recovery (via [`WalWriter`]/[`WalReader`]), and two-layer telemetry
//! (hardware + process tracking) for persistent machines.
//!
//! # Architecture
//!
//! - **Capabilities** — Pluggable operations implementing the [`Capability`] trait
//! - **Jobs** — Lifecycle-tracked execution units ([`Job`], [`JobState`])
//! - **Telemetry** — Hardware awareness ([`Telemetry`])
//! - **Process Snapshot** — Running process awareness ([`ProcessSnapshot`])
//! - **WAL** — Append-only crash recovery log
//! - **Backup** — Undo support via pre-mutation file backups
//!
//! # Example
//!
//! ```rust
//! use runtimo_core::{FileRead, Capability, Context};
//! use serde_json::json;
//!
//! let cap = FileRead;
//! assert_eq!(cap.name(), "FileRead");
//! ```

pub mod backup;
pub mod capabilities;
pub mod capability;
pub mod cmd;
pub mod executor;
pub mod job;
pub mod llmosafe;
pub mod processes;
pub mod schema;
pub mod telemetry;
pub mod validation;
pub mod wal;

pub use backup::BackupManager;
pub use capabilities::{FileRead, FileWrite};
pub use capability::{Capability, CapabilityRegistry, Context, Output};
pub use executor::{execute_with_telemetry, ExecutionResult};
pub use job::{Job, JobId, JobState};
pub use llmosafe::LlmoSafeGuard;
pub use processes::ProcessSnapshot;
pub use schema::SchemaValidator;
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
.or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/share")))
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
}
