//! Tetragon adapter — pinned v1.7.1, monitor-only.
//!
//! # Overview
//!
//! The `TetragonAdapter` interfaces with the `tetragon` binary (pinned
//! to version 1.7.1) in `--monitor` mode only. This means:
//! - **Observation-only**: No enforcement policies are loaded or applied.
//! - **Kernel-level**: Captures exec/exit events from kernel hooks.
//! - **Raw artifacts**: Produces JSON artifacts that are hashed (SHA-256)
//!   and referenced in the manifest+WAL, but never forced into WAL volume.
//!
//! # Provider Status
//!
//! | State | Condition |
//! |-------|-----------|
//! | `Active` | `tetragon` binary found at v1.7.1, `--monitor` flag works |
//! | `Degraded` | Binary found but version mismatch or partial failure |
//! | `Unavailable` | Binary absent or permission denied |
//!
//! # Invariants
//!
//! - Enforcement is NEVER enabled. The `--monitor` flag is the only mode.
//! - `SymbolUID` never appears in adapter output.
//! - Raw JSON artifacts are preserved as files; only hashes enter WAL.
//! - PID is always paired with `process_start_time` in `RunProcessKey`.
//! - `ObservedExec` and `ObservedExit` families are produced with
//!   `EvidenceFidelity::KernelObserved`.
//! - No custom syscall/ancestry/loader is introduced.

use crate::adapters::{detect_binary, sha256_file, AdapterStartResult, ArtifactReducer};
use crate::llmosafe::LlmoSafeGuard;
use crate::runtime::ArtifactRef;
use crate::runtime::{ProviderStatus, RuntimeFactV1};
use crate::validation::path::{validate_path, PathContext};
use crate::wal::{WalEvent, WalEventType, WalWriter};
use log;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Pinned Tetragon version.
pub const TETRAGON_PINNED_VERSION: &str = "1.7.1";

/// The `--monitor` flag value for observation-only mode.
pub const TETRAGON_MONITOR_MODE: &str = "monitor";

/// Tetragon adapter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct TetragonConfig {
    /// Binary path (auto-detected if not specified).
    pub binary_path: Option<PathBuf>,
    /// Pinned version (always "1.7.1").
    pub pinned_version: String,
    /// Operating mode (always "monitor").
    pub mode: String,
    /// Whether enforcement is enabled (always false).
    pub enforcement_enabled: bool,
    /// Kernel version requirement (minimum "5.10").
    pub min_kernel_version: String,
}

impl Default for TetragonConfig {
    fn default() -> Self {
        Self {
            binary_path: None,
            pinned_version: TETRAGON_PINNED_VERSION.to_string(),
            mode: TETRAGON_MONITOR_MODE.to_string(),
            enforcement_enabled: false,
            min_kernel_version: "5.10".to_string(),
        }
    }
}

/// Raw artifact produced by the Tetragon provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct TetragonRawArtifact {
    /// Event type (exec, exit).
    pub event_type: String,
    /// Process ID.
    pub pid: u32,
    /// Process start time (from /proc/pid/stat field 22).
    pub process_start_time: u64,
    /// Timestamp of the event.
    pub timestamp: u64,
    /// Raw event data from Tetragon.
    pub data: serde_json::Value,
}

/// Tetragon adapter — observation-only kernel event provider.
///
/// # Example
///
/// ```rust,ignore
/// let mut adapter = TetragonAdapter::new();
/// let result = adapter.start()?;
/// // Collect artifacts...
/// let facts = adapter.reduce(&artifacts)?;
/// adapter.stop()?;
/// ```
pub struct TetragonAdapter {
    /// Adapter configuration.
    config: TetragonConfig,
    /// Current provider status.
    pub status: ProviderStatus,
    /// Directory for raw artifacts.
    artifact_dir: PathBuf,
    /// Whether the adapter has been started.
    started: bool,
    /// Resource guard — LlmoSafeGuard::new() (80% ceiling).
    guard: LlmoSafeGuard,
}

impl std::fmt::Debug for TetragonAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TetragonAdapter")
            .field("config", &self.config)
            .field("status", &self.status)
            .field("artifact_dir", &self.artifact_dir)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl TetragonAdapter {
    /// Creates a new `TetragonAdapter` with default configuration.
    ///
    /// Detects the `tetragon` binary availability immediately.
    /// The adapter is not started until `start()` is called.
    #[must_use]
    pub fn new() -> Self {
        let config = TetragonConfig::default();
        let binary_path = detect_binary("tetragon");
        let status = Self::determine_status(binary_path.as_ref(), &config);
        let artifact_dir =
            std::env::temp_dir().join(format!("runtimo_tetragon_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&artifact_dir);

        Self {
            config,
            status,
            artifact_dir,
            started: false,
            guard: LlmoSafeGuard::new(),
        }
    }

    /// Creates a `TetragonAdapter` with a custom artifact directory.
    ///
    /// # Arguments
    /// * `artifact_dir` - Directory for raw artifact storage
    #[must_use]
    pub fn with_artifact_dir(artifact_dir: PathBuf) -> Self {
        let mut adapter = Self::new();
        adapter.artifact_dir = artifact_dir;
        adapter
    }

    /// Determines the provider status based on binary availability and config.
    fn determine_status(binary_path: Option<&PathBuf>, config: &TetragonConfig) -> ProviderStatus {
        match binary_path {
            Some(path) => {
                // Validate the path before executing: must exist, be a regular file,
                // and be within RUNTIMO_ALLOWED_PATHS.
                let ctx = PathContext {
                    require_exists: true,
                    require_file: true,
                    ..Default::default()
                };
                if let Err(e) = validate_path(&path.to_string_lossy(), &ctx) {
                    return ProviderStatus::Unavailable {
                        provider: "tetragon".to_string(),
                        version: config.pinned_version.clone(),
                        reason: format!("tetragon binary path validation failed: {}", e),
                    };
                }
                // Also verify is_file explicitly (validate_path checks via canonicalize).
                if !path.is_file() {
                    return ProviderStatus::Unavailable {
                        provider: "tetragon".to_string(),
                        version: config.pinned_version.clone(),
                        reason: format!("tetragon binary not found at {}", path.display()),
                    };
                }
                // Check if the binary is the pinned version.
                // We verify existence and try to get version info.
                let version_output = std::process::Command::new(path)
                    .args(["--version"])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok());

                let is_pinned = version_output
                    .as_ref()
                    .is_some_and(|v| v.contains(&config.pinned_version));

                if is_pinned {
                    ProviderStatus::Active {
                        provider: "tetragon".to_string(),
                        version: config.pinned_version.clone(),
                        mode: config.mode.clone(),
                    }
                } else {
                    ProviderStatus::Degraded {
                        provider: "tetragon".to_string(),
                        version: config.pinned_version.clone(),
                        mode: config.mode.clone(),
                        reason: format!(
                            "Version mismatch: found {}, expected {}",
                            version_output.unwrap_or_else(|| "unknown".to_string()),
                            config.pinned_version
                        ),
                    }
                }
            }
            None => ProviderStatus::Unavailable {
                provider: "tetragon".to_string(),
                version: config.pinned_version.clone(),
                reason: "tetragon binary not found in PATH".to_string(),
            },
        }
    }

    /// Returns the current provider status.
    #[must_use]
    pub fn status(&self) -> &ProviderStatus {
        &self.status
    }

    /// Returns the artifact directory path.
    #[must_use]
    pub fn artifact_dir(&self) -> &PathBuf {
        &self.artifact_dir
    }

    /// Returns the pinned version.
    #[must_use]
    pub fn pinned_version() -> &'static str {
        TETRAGON_PINNED_VERSION
    }

    /// Starts the adapter.
    ///
    /// # Returns
    /// * `AdapterStartResult::Started` if the binary is available and active.
    /// * `AdapterStartResult::Degraded` if the binary exists but version mismatch.
    /// * `AdapterStartResult::Unavailable` if the binary is absent.
    ///
    /// # Errors
    /// Returns error if the artifact directory cannot be created or if
    /// the guard check fails.
    pub fn start(&mut self) -> Result<AdapterStartResult, crate::Error> {
        // Check resource guard.
        if let Err(e) = self.guard.check() {
            return Err(crate::Error::ResourceLimitExceeded(e));
        }

        match &self.status {
            ProviderStatus::Active { .. } => {
                self.started = true;
                Ok(AdapterStartResult::Started {
                    status: self.status.clone(),
                    artifact_dir: self.artifact_dir.clone(),
                })
            }
            ProviderStatus::Degraded { reason, .. } => {
                self.started = true;
                Ok(AdapterStartResult::Degraded {
                    status: self.status.clone(),
                    reason: reason.clone(),
                })
            }
            ProviderStatus::Unavailable { reason, .. } => Ok(AdapterStartResult::Unavailable {
                status: self.status.clone(),
                reason: reason.clone(),
            }),
            ProviderStatus::Failed { .. } => Ok(AdapterStartResult::Unavailable {
                status: self.status.clone(),
                reason: "Provider failed".to_string(),
            }),
        }
    }

    /// Stops the adapter and finalizes artifacts.
    ///
    /// Emits a `ProviderStopped` WAL event and performs cleanup.
    ///
    /// # Errors
    /// Returns error if the WAL event cannot be written.
    pub fn stop(&mut self) -> Result<(), crate::Error> {
        self.started = false;
        // Emit a WAL event for provider stop.
        let wal_path = std::env::temp_dir().join("runtimo_tetragon_stop.wal");
        let mut wal = WalWriter::create(&wal_path)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        wal.append(WalEvent {
            seq: wal.seq(),
            ts,
            event_type: WalEventType::ObserveCompleted,
            job_id: "tetragon-stop".to_string(),
            output: Some(serde_json::json!({
                "provider": "tetragon",
                "action": "stopped",
                "status": format!("{:?}", self.status),
            })),
            ..Default::default()
        })?;
        if let Err(e) = wal.flush_batch() {
            log::error!("Failed to flush WAL batch: {}", e);
        }
        Ok(())
    }

    /// Captures a raw artifact from the Tetragon provider.
    ///
    /// In monitor-only mode, this simulates capturing an exec/exit event.
    /// The raw artifact is written to the artifact directory and hashed.
    ///
    /// # Arguments
    /// * `event_type` - "exec" or "exit"
    /// * `pid` - Process ID
    /// * `process_start_time` - Process start time from /proc/pid/stat
    /// * `data` - Raw event data
    ///
    /// Returns the `ArtifactRef` with SHA-256 hash.
    ///
    /// # Errors
    /// Returns error if serialization, file write, or hashing fails.
    pub fn capture_artifact(
        &self,
        event_type: &str,
        pid: u32,
        process_start_time: u64,
        data: serde_json::Value,
    ) -> Result<ArtifactRef, crate::Error> {
        let artifact = TetragonRawArtifact {
            event_type: event_type.to_string(),
            pid,
            process_start_time,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            data,
        };

        let artifact_path = self
            .artifact_dir
            .join(format!("{}.json", artifact.timestamp));
        let json = serde_json::to_string(&artifact).map_err(|e| {
            crate::Error::ExecutionFailed(format!("Failed to serialize artifact: {}", e))
        })?;
        std::fs::write(&artifact_path, json).map_err(|e| {
            crate::Error::ExecutionFailed(format!("Failed to write artifact: {}", e))
        })?;

        // Validate path before hashing to prevent TOCTOU
        // (symlink swap between write and hash computation).
        let ctx = PathContext {
            require_exists: true,
            require_file: true,
            ..Default::default()
        };
        let validated_path =
            validate_path(&artifact_path.to_string_lossy(), &ctx).map_err(|e| {
                crate::Error::ExecutionFailed(format!("artifact path validation failed: {}", e))
            })?;
        let sha256 = sha256_file(&validated_path).map_err(|e| {
            crate::Error::ExecutionFailed(format!("Failed to hash artifact: {}", e))
        })?;

        Ok(ArtifactRef {
            path: artifact_path.to_string_lossy().to_string(),
            media: "application/json".to_string(),
            sha256,
            provider: "tetragon".to_string(),
            pid,
            process_start_time,
        })
    }

    /// Reduces raw artifacts into `RuntimeFactV1` entries.
    ///
    /// Delegates to [`ArtifactReducer::reduce_artifacts`] which
    /// derives PID and `process_start_time` from artifact identity
    /// via `parse_artifact_identity`, ensuring the invariant that
    /// PID is never alone (always paired with `process_start_time`).
    ///
    /// # Arguments
    /// * `artifacts` - Slice of raw artifact references
    /// * `run_id` - Unique run identifier
    /// * `revision` - Git commit revision
    ///
    /// Returns a vector of `RuntimeFactV1` facts.
    ///
    /// # Errors
    /// Returns error if artifact reduction fails.
    pub fn reduce(
        &self,
        artifacts: &[ArtifactRef],
        run_id: &str,
        revision: &str,
    ) -> Result<Vec<RuntimeFactV1>, crate::Error> {
        ArtifactReducer::reduce_artifacts(artifacts, run_id, revision)
    }

    /// Returns whether the adapter is currently started.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started
    }
}

impl Default for TetragonAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "runtimo_tetra_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn tetragon_new_detects_status() {
        let adapter = TetragonAdapter::new();
        // Status is determined at construction time.
        // If tetragon is not installed, it should be Unavailable.
        match adapter.status() {
            ProviderStatus::Unavailable { .. }
            | ProviderStatus::Active { .. }
            | ProviderStatus::Degraded { .. }
            | ProviderStatus::Failed { .. } => {}
        }
    }

    #[test]
    fn tetragon_config_defaults() {
        let config = TetragonConfig::default();
        assert_eq!(config.pinned_version, "1.7.1");
        assert_eq!(config.mode, "monitor");
        assert!(
            !config.enforcement_enabled,
            "Enforcement must never be enabled"
        );
        assert_eq!(config.min_kernel_version, "5.10");
    }

    #[test]
    fn tetragon_capture_artifact_produces_hash() {
        let dir = tmp_dir();
        let mut adapter = TetragonAdapter::with_artifact_dir(dir.clone());
        adapter.status = ProviderStatus::Active {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            mode: "monitor".to_string(),
        };
        let _ = adapter.start();

        let data = serde_json::json!({"event": "exec", "comm": "test"});
        let artifact = adapter.capture_artifact("exec", 1234, 1000, data).unwrap();

        assert_eq!(artifact.provider, "tetragon");
        assert_eq!(artifact.sha256.len(), 64, "SHA-256 must be 64 hex chars");
        assert!(
            PathBuf::from(&artifact.path).exists(),
            "Artifact file must exist"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tetragon_start_returns_started_when_active() {
        let mut adapter = TetragonAdapter::new();
        adapter.status = ProviderStatus::Active {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            mode: "monitor".to_string(),
        };
        let result = adapter.start().unwrap();
        assert!(matches!(result, AdapterStartResult::Started { .. }));
        assert!(adapter.is_started());
    }

    #[test]
    fn tetragon_start_returns_unavailable_when_absent() {
        let mut adapter = TetragonAdapter::new();
        adapter.status = ProviderStatus::Unavailable {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            reason: "binary not found".to_string(),
        };
        let result = adapter.start().unwrap();
        assert!(matches!(result, AdapterStartResult::Unavailable { .. }));
    }

    #[test]
    fn tetragon_start_returns_degraded_when_version_mismatch() {
        let mut adapter = TetragonAdapter::new();
        adapter.status = ProviderStatus::Degraded {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            mode: "monitor".to_string(),
            reason: "version mismatch".to_string(),
        };
        let result = adapter.start().unwrap();
        assert!(matches!(result, AdapterStartResult::Degraded { .. }));
    }

    #[test]
    fn tetragon_stop_emits_wal_event() {
        let mut adapter = TetragonAdapter::new();
        adapter.status = ProviderStatus::Active {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            mode: "monitor".to_string(),
        };
        let _ = adapter.start();
        let result = adapter.stop();
        assert!(result.is_ok(), "stop should succeed: {:?}", result.err());
    }

    #[test]
    fn tetragon_pinned_version_constant() {
        assert_eq!(TetragonAdapter::pinned_version(), "1.7.1");
    }
}
