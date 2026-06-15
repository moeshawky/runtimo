//! Runtimo Core — Agent-centric capability runtime.
//!
//! Runtimo provides structured execution, resource limits, crash recovery,
//! and two-layer telemetry (hardware + process tracking) for machines that
//! cannot be factory-reset. Every capability execution captures before/after
//! snapshots, with full audit trails and undo support.
//!
//! # Architecture
//!
//! - **Capabilities** — Pluggable operations implementing the [`Capability`] trait
//! - **Jobs** — Lifecycle-tracked execution units (Job, [`JobState`])
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

// Allow idiomatic test lints in test mode as panic/unwrap/indexing are standard in tests.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unused_result_ok
    )
)]

pub mod backup;
/// Pluggable capability implementations (file I/O, shell, git, etc.).
pub mod capabilities;
/// Core trait and registry for pluggable operations.
pub mod capability;
/// Shell command execution helper.
pub mod cmd;
/// Global configuration and path resolution.
pub mod config;
/// Capability executor with telemetry and safety guards.
pub mod executor;
/// Job identity, state machine, and WAL event types.
pub mod job;
/// LLM safety guard — CPU/RAM circuit breakers and entropy source.
pub mod llmosafe;
/// Health monitoring with alerting.
pub mod monitor;
/// Process snapshot, zombie detection, and top-N queries.
pub mod processes;
/// Session tracking for reliable SSH.
pub mod session;
/// System telemetry capture and reporting.
pub mod telemetry;
/// Path validation against allowed-prefix lists.
pub mod validation;
/// Write-ahead log for crash recovery.
pub mod wal;

pub use backup::BackupManager;
pub use capabilities::{FileRead, FileWrite, GitExec, Kill, ShellExec, Undo};
pub use capability::{Capability, CapabilityRegistry, Context, Output};
pub use config::RuntimoConfig;
pub use executor::{execute_with_telemetry, execute_with_telemetry_and_session};
pub use job::{Job, JobId, JobState};
pub use llmosafe::LlmoSafeGuard;
pub use monitor::HealthMonitor;
pub use processes::ProcessSnapshot;
pub use telemetry::Telemetry;
pub use wal::{WalEvent, WalEventType, WalReader, WalWriter};

/// Error types for runtimo-core.
///
/// Covers all failure modes: state transitions, schema validation,
/// capability execution, WAL/backup errors, resource limits, and telemetry.
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking regardless
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

    /// Session operation failed (create, load, save, list).
    #[error("Session error: {0}")]
    SessionError(String),

    /// System resource limit exceeded (CPU, RAM, or zombie count).
    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    /// Telemetry capture failed.
    #[error("Telemetry error: {0}")]
    TelemetryError(String),

    /// Cognitive safety violation detected by LLMOSafe.
    #[error("Cognitive safety violation: {0}")]
    CognitiveSafetyViolation(String),
}

/// Result alias for runtimo-core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Utility functions for path management.
pub mod utils {
    use std::path::PathBuf;

    /// Returns the data directory following XDG spec.
    ///
    /// Uses `XDG_DATA_HOME` if set, otherwise `~/.local/share/runtimo`.
    ///
    /// Falls back to `/tmp/runtimo` with a stderr warning when neither
    /// `XDG_DATA_HOME` nor `HOME` is set. Data in `/tmp` is not persistent
    /// across reboots — WAL and backup durability guarantees are degraded
    /// in this fallback mode.
    pub fn data_dir() -> PathBuf {
        let base = std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/share"))
            });
        if let Some(dir) = base {
            dir.join("runtimo")
        } else {
            eprintln!(
                "[runtimo] Warning: XDG_DATA_HOME and HOME unset — using /tmp/runtimo \
                 (data will not survive reboot)"
            );
            PathBuf::from("/tmp/runtimo")
        }
    }

    /// Returns the WAL path (env override or default).
    pub fn wal_path() -> PathBuf {
        std::env::var("RUNTIMO_WAL_PATH")
            .map_or_else(|_| data_dir().join("wal.jsonl"), PathBuf::from)
    }

    /// Returns the backup directory (env override or default).
    pub fn backup_dir() -> PathBuf {
        std::env::var("RUNTIMO_BACKUP_DIR")
            .map_or_else(|_| data_dir().join("backups"), PathBuf::from)
    }

    /// Generates a unique ID from 16 random bytes (32 hex chars).
    ///
    /// Uses `/dev/urandom` for collision resistance — P(collision) < 10⁻¹⁵
    /// even at 100 IDs/sec for 1 hour. Falls back to timestamp if urandom
    /// is unavailable (e.g., non-Linux platforms).
    #[must_use]
    pub fn generate_id() -> String {
        let mut bytes = [0u8; 16];
        if std::fs::File::open("/dev/urandom")
            .ok()
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes).ok())
            .is_some()
        {
            #[allow(clippy::format_collect)]
            bytes.iter().map(|b| format!("{b:02x}")).collect()
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
