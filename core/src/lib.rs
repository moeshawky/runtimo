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

/// Versioned runtime contracts — taxonomy + provenance, not acquisition.
pub mod runtime;

/// Provider adapters — Tetragon + JFR with raw artifact custody.
pub mod adapters;
/// Undo support via pre-mutation file backups.
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
/// Observe subsystem — sampling, bundling, budgeting, auditing.
pub mod observe;
/// Oracle — property specification and evaluation.
pub mod oracle;
/// Process snapshot, zombie detection, and top-N queries.
pub mod processes;
/// Thin LLMOSafe 0.9 conformance boundary: assessment, disposition, input semantics.
pub mod safety;
/// Session tracking for reliable SSH.
pub mod session;
/// System telemetry capture and reporting.
pub mod telemetry;
/// Path validation against allowed-prefix lists.
pub mod validation;
/// Write-ahead log for crash recovery.
pub mod wal;

pub use oracle::{
    evaluate, evaluate_v2, Op, OracleError, Predicate, PropertySpec, PropertyVerdict, Quantifier,
    Verdict,
};

pub use adapters::{ArtifactReducer, JfrAdapter, JfrConfig, TetragonAdapter, TetragonConfig};
pub use backup::BackupManager;
pub use capabilities::{Delete, FileRead, FileWrite, GitExec, Kill, ShellExec, Undo};
pub use capability::{
    Capability, CapabilityError, CapabilityRegistry, Context, Output, TypedCapability,
};
pub use config::RuntimoConfig;
pub use executor::{execute_with_telemetry, execute_with_telemetry_and_session};
pub use job::{Job, JobId, JobState};
pub use llmosafe::LlmoSafeGuard;
pub use monitor::HealthMonitor;
pub use processes::ProcessSnapshot;
pub use runtime::{
    resolve_locator, EvidenceFidelity, ProviderStatus, RunManifestV1, RunProcessKey,
    RuntimeFactExport, RuntimeFactV1, RuntimeLocator, SymbolUID, SymbolUidResolution,
};
pub use safety::{
    AnalysisKind, AssessmentError, FieldSemantics, InputClass, RuntimoDisposition,
    SafetyAssessmentV1, SAFETY_SCHEMA_VERSION,
};
pub use telemetry::Telemetry;
pub use validation::{validate_path, PathContext};
pub use wal::{WalEvent, WalEventType, WalReader, WalWriter};

/// Error types for runtimo-core.
///
/// Covers all failure modes: state transitions, schema validation,
/// capability execution, WAL/backup errors, resource limits, and telemetry.
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking regardless
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid job state transition attempted.
    ///
    /// Reserved typed channel for state-machine errors. [`Job::transition_to`]
    /// currently reports transition failures as formatted strings for
    /// backwards compatibility; this variant is constructed by future
    /// callers that need structured `from`/`to` data.
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

    /// Execution failed with a structured capability error.
    ///
    /// This variant preserves the original `CapabilityError` variant information
    /// that would otherwise be lost to stringification in the blanket impl at
    /// `capability.rs:431`. Clients can match on this variant to programmatically
    /// distinguish `PermissionDenied` from `NotFound`, `InvalidArgs`, etc.
    ///
    /// # Fields
    /// - `msg`: Human-readable error message (for display/logging)
    /// - `variant`: Machine-readable variant name (for programmatic handling)
    /// - `code`: JSON-RPC error code in range -32000 to -32099 (server-defined errors)
    ///
    /// # Example
    /// ```rust,ignore
    /// match error {
    ///     Error::CapabilityExecutionFailed { code, variant, msg } => {
    ///         eprintln!("Error {}: {} - {}", code, variant, msg);
    ///     }
    ///     _ => {}
    /// }
    /// ```
    #[error("Capability execution failed: {variant} - {msg}")]
    CapabilityExecutionFailed {
        msg: String,
        variant: &'static str,
        code: i32,
    },

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
    ///
    /// Legacy generic channel — preserved for backwards compatibility.
    /// New code emits the typed variants below (`SafetyEscalationRequired`,
    /// `SafetyRejected`, `SafetyFatal`, `SafetyAnalysisFailed`) so callers
    /// can distinguish escalation from hard rejection without string parsing.
    #[error("Cognitive safety violation: {0}")]
    CognitiveSafetyViolation(String),

    /// Semantic Escalate: do not execute now; higher-level handler required.
    /// Distinct from hard Halt — preserved through audit + Oracle.
    #[error("Safety escalation required: {0}")]
    SafetyEscalationRequired(String),

    /// Hard safety rejection (upstream Halt).
    #[error("Safety rejected: {0}")]
    SafetyRejected(String),

    /// Fatal-class upstream safety result (upstream Exit).
    /// Does NOT terminate the daemon/process by itself.
    #[error("Safety fatal: {0}")]
    SafetyFatal(String),

    /// Upstream analysis could not complete (`SiftError`).
    /// Never coerced to allow.
    #[error("Safety analysis failed: {0}")]
    SafetyAnalysisFailed(String),
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
            .filter(|p| std::path::Path::new(p).is_absolute())
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

    /// Returns the backup directory derived from `data_dir()`.
    ///
    /// Always returns `data_dir().join("backups")`. This is a derived path
    /// from the trusted `data_dir` root — no env var override is available
    /// (see ADR-C28). External config of the backup location would create
    /// an attacker control vector.
    #[must_use]
    pub fn backup_dir() -> PathBuf {
        data_dir().join("backups")
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

/// Shared environment guard for serializing `RUNTIMO_TEST_PRESSURE`
/// mutations across test modules.
#[allow(dead_code)]
pub(crate) static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the environment guard lock, serializing `RUNTIMO_TEST_PRESSURE`
/// mutations across all test modules.
#[allow(dead_code)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII guard that captures four `RUNTIMO_*` env vars on construction
/// and restores them on drop (unset-vs-set semantics).
///
/// Captures: `RUNTIMO_TEST_PRESSURE`, `RUNTIMO_MEMORY_CEILING_BYTES`,
/// `RUNTIMO_DAL`, `RUNTIMO_SEMANTIC_POLICY`.
///
/// On construction, acquires `ENV_GUARD` and holds it for the
/// guard's entire lifetime — closing the mutation window for the
/// duration of the test. On drop: if a var was `None` (absent) at
/// construction time, it is removed; if it was `Some(v)`, it is
/// restored to `v`. This ensures test mutations never leak into
/// sibling tests, even when a panic occurs during the test body.
///
/// # Example
///
/// ```rust,ignore
/// let _guard = crate::test_isolation::EnvGuard::new();
/// std::env::set_var("RUNTIMO_TEST_PRESSURE", "90");
/// // ... test code ...
/// // Drop restores the original state automatically.
/// ```
pub mod test_isolation {
    use std::env;

    /// Captures four `RUNTIMO_*` env vars and holds the global lock
    /// for the guard's entire lifetime.
    ///
    /// The lock serializes env-var mutations across test modules.
    /// Each captured value is stored as `Some(v)` if present,
    /// or `None` if the var was absent. The mutex is held until
    /// `Drop` restores the vars, closing the mutation window.
    #[derive(Debug)]
    pub struct EnvGuard {
        #[allow(dead_code)]
        guard: std::sync::MutexGuard<'static, ()>,
        pressure: Option<String>,
        ceiling: Option<String>,
        dal: Option<String>,
        semantic_policy: Option<String>,
    }

    impl Default for EnvGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EnvGuard {
        /// Capture the four env vars and acquire the global lock,
        /// holding it for the entire lifetime of this guard.
        ///
        /// The lock serializes env-var mutations across test modules.
        /// Each captured value is stored as `Some(v)` if present,
        /// or `None` if the var was absent. The mutex is held until
        /// `Drop` restores the vars, closing the mutation window.
        #[must_use]
        pub fn new() -> Self {
            let guard = crate::lock_env();
            let pressure = env::var("RUNTIMO_TEST_PRESSURE").ok();
            let ceiling = env::var("RUNTIMO_MEMORY_CEILING_BYTES").ok();
            let dal = env::var("RUNTIMO_DAL").ok();
            let semantic_policy = env::var("RUNTIMO_SEMANTIC_POLICY").ok();
            // Hold the guard for the lifetime of EnvGuard — this
            // closes the mutation window for the entire test body.
            Self {
                guard,
                pressure,
                ceiling,
                dal,
                semantic_policy,
            }
        }

        /// Returns the captured `RUNTIMO_TEST_PRESSURE` value, if any.
        #[must_use]
        pub fn pressure(&self) -> Option<&str> {
            self.pressure.as_deref()
        }

        /// Returns the captured `RUNTIMO_MEMORY_CEILING_BYTES` value, if any.
        #[must_use]
        pub fn ceiling(&self) -> Option<&str> {
            self.ceiling.as_deref()
        }

        /// Returns the captured `RUNTIMO_DAL` value, if any.
        #[must_use]
        pub fn dal(&self) -> Option<&str> {
            self.dal.as_deref()
        }

        /// Returns the captured `RUNTIMO_SEMANTIC_POLICY` value, if any.
        #[must_use]
        pub fn semantic_policy(&self) -> Option<&str> {
            self.semantic_policy.as_deref()
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // The mutex is already held by `self.guard`; no
            // re-acquisition needed (would deadlock). Drop the
            // guard after restoring vars to release the lock.
            match &self.pressure {
                Some(v) => env::set_var("RUNTIMO_TEST_PRESSURE", v),
                None => env::remove_var("RUNTIMO_TEST_PRESSURE"),
            }
            match &self.ceiling {
                Some(v) => env::set_var("RUNTIMO_MEMORY_CEILING_BYTES", v),
                None => env::remove_var("RUNTIMO_MEMORY_CEILING_BYTES"),
            }
            match &self.dal {
                Some(v) => env::set_var("RUNTIMO_DAL", v),
                None => env::remove_var("RUNTIMO_DAL"),
            }
            match &self.semantic_policy {
                Some(v) => env::set_var("RUNTIMO_SEMANTIC_POLICY", v),
                None => env::remove_var("RUNTIMO_SEMANTIC_POLICY"),
            }
        }
    }
}
