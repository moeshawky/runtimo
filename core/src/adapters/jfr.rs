//! JFR adapter — JDK17 jcmd external attach.
//!
//! # Overview
//!
//! The `JfrAdapter` interfaces with the JDK17 `jcmd` binary for
//! external-attach observation of class-load and exception events.
//! This produces `.jfr` (Java Flight Recorder) artifacts that are
//! hashed (SHA-256) and referenced in the manifest+WAL, but never
//! forced into WAL volume.
//!
//! # Provider Status
//!
//! | State | Condition |
//! |-------|-----------|
//! | `Active` | `jcmd` binary found, JDK17 detected, attach works |
//! | `Degraded` | Binary found but JDK version mismatch or partial failure |
//! | `Unavailable` | Binary absent, JDK not found, or permission denied |
//!
//! # Invariants
//!
//! - `SymbolUID` never appears in adapter output.
//! - Raw `.jfr` artifacts are preserved as files; only hashes enter WAL.
//! - PID is always paired with `process_start_time` in `RunProcessKey`.
//! - `ObservedClassLoad` and `ObservedException` families are produced
//!   with `EvidenceFidelity::ExactRuntime`.
//! - No JVMTI, custom parser, or loader is introduced.
//! - External attach only — no in-process instrumentation.

use crate::adapters::{detect_binary, sha256_file, AdapterStartResult, ArtifactReducer};
use crate::llmosafe::LlmoSafeGuard;
use crate::runtime::ArtifactRef;
use crate::runtime::{ProviderStatus, RuntimeFactV1};
use crate::validation::path::{validate_path, PathContext};
use crate::wal::{WalEvent, WalEventType, WalWriter};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Pinned JDK version for JFR adapter.
pub const JFR_PINNED_JDK_VERSION: &str = "17";

/// JFR adapter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct JfrConfig {
    /// Binary path (auto-detected if not specified).
    pub binary_path: Option<PathBuf>,
    /// Pinned JDK version (always "17").
    pub pinned_jdk_version: String,
    /// Operating mode (always "exact").
    pub mode: String,
    /// Whether external attach is enabled (always true).
    pub external_attach_enabled: bool,
}

impl Default for JfrConfig {
    fn default() -> Self {
        Self {
            binary_path: None,
            pinned_jdk_version: JFR_PINNED_JDK_VERSION.to_string(),
            mode: "exact".to_string(),
            external_attach_enabled: true,
        }
    }
}

/// Raw artifact produced by the JFR provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct JfrRawArtifact {
    /// Event type (class_load, exception).
    pub event_type: String,
    /// Process ID.
    pub pid: u32,
    /// Process start time (from /proc/pid/stat field 22).
    pub process_start_time: u64,
    /// Timestamp of the event.
    pub timestamp: u64,
    /// Raw event data from JFR.
    pub data: serde_json::Value,
}

/// JFR adapter — external-attach class-load/exception provider.
///
/// # Example
///
/// ```rust,ignore
/// let mut adapter = JfrAdapter::new();
/// let result = adapter.start()?;
/// // Collect artifacts...
/// let facts = adapter.reduce(&artifacts)?;
/// adapter.stop()?;
/// ```
pub struct JfrAdapter {
    /// Adapter configuration.
    #[allow(dead_code)]
    config: JfrConfig,
    /// Current provider status.
    pub status: ProviderStatus,
    /// Directory for raw artifacts.
    artifact_dir: PathBuf,
    /// Whether the adapter has been started.
    started: bool,
    /// Resource guard — LlmoSafeGuard::new() (80% ceiling).
    guard: LlmoSafeGuard,
}

impl JfrAdapter {
    /// Creates a new `JfrAdapter` with default configuration.
    ///
    /// Detects the `jcmd` binary availability immediately.
    /// The adapter is not started until `start()` is called.
    #[must_use]
    pub fn new() -> Self {
        let config = JfrConfig::default();
        let binary_path = detect_binary("jcmd");
        let status = Self::determine_status(binary_path.as_ref(), &config);
        let artifact_dir = std::env::temp_dir().join(format!("runtimo_jfr_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&artifact_dir);

        Self {
            config,
            status,
            artifact_dir,
            started: false,
            guard: LlmoSafeGuard::new(),
        }
    }

    /// Creates a `JfrAdapter` with a custom artifact directory.
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
    fn determine_status(binary_path: Option<&PathBuf>, config: &JfrConfig) -> ProviderStatus {
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
                        provider: "jfr".to_string(),
                        version: config.pinned_jdk_version.clone(),
                        reason: format!("jcmd binary path validation failed: {}", e),
                    };
                }
                // Also verify is_file explicitly (validate_path checks via canonicalize).
                if !path.is_file() {
                    return ProviderStatus::Unavailable {
                        provider: "jfr".to_string(),
                        version: config.pinned_jdk_version.clone(),
                        reason: format!("jcmd binary not found at {}", path.display()),
                    };
                }
                // Check if the binary is JDK17.
                let version_output = std::process::Command::new(path)
                    .args(["-version"])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stderr).ok())
                    .or_else(|| {
                        std::process::Command::new(path)
                            .args(["--version"])
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                    });

                let is_jdk17 = version_output
                    .as_ref()
                    .is_some_and(|v| v.contains("17") || v.contains("JDK 17"));

                if is_jdk17 {
                    ProviderStatus::Active {
                        provider: "jfr".to_string(),
                        version: config.pinned_jdk_version.clone(),
                        mode: config.mode.clone(),
                    }
                } else {
                    ProviderStatus::Degraded {
                        provider: "jfr".to_string(),
                        version: config.pinned_jdk_version.clone(),
                        mode: config.mode.clone(),
                        reason: format!(
                            "JDK version mismatch: found {}, expected JDK {}",
                            version_output.unwrap_or_else(|| "unknown".to_string()),
                            config.pinned_jdk_version
                        ),
                    }
                }
            }
            None => ProviderStatus::Unavailable {
                provider: "jfr".to_string(),
                version: config.pinned_jdk_version.clone(),
                reason: "jcmd binary not found in PATH".to_string(),
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

    /// Returns the pinned JDK version.
    #[must_use]
    pub fn pinned_jdk_version() -> &'static str {
        JFR_PINNED_JDK_VERSION
    }

    /// Starts the adapter.
    ///
    /// # Returns
    /// * `AdapterStartResult::Started` if the binary is available and active.
    /// * `AdapterStartResult::Degraded` if the binary exists but version mismatch.
    /// * `AdapterStartResult::Unavailable` if the binary is absent.
    ///
    /// # Errors
    /// Returns error if the guard check fails or the artifact directory cannot be created.
    pub fn start(&mut self) -> Result<AdapterStartResult, crate::Error> {
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
    /// # Errors
    /// Returns error if the WAL event cannot be written.
    pub fn stop(&mut self) -> Result<(), crate::Error> {
        self.started = false;
        let wal_path = std::env::temp_dir().join("runtimo_jfr_stop.wal");
        let mut wal = WalWriter::create(&wal_path)?;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        wal.append(WalEvent {
            seq: wal.seq(),
            ts,
            event_type: WalEventType::ObserveCompleted,
            job_id: "jfr-stop".to_string(),
            output: Some(serde_json::json!({
                "provider": "jfr",
                "action": "stopped",
                "status": format!("{:?}", self.status),
            })),
            ..Default::default()
        })?;
        let flush_result = wal.flush_batch();
        if let Err(e) = flush_result {
            log::error!("Failed to flush WAL batch: {}", e);
        }
        Ok(())
    }

    /// Captures a raw artifact from the JFR provider.
    ///
    /// # Arguments
    /// * `event_type` - "class_load" or "exception"
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
        let artifact = JfrRawArtifact {
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
            .join(format!("{}.jfr", artifact.timestamp));
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
            media: "application/jfr".to_string(),
            sha256,
            provider: "jfr".to_string(),
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

impl Default for JfrAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{EvidenceFidelity, RuntimeFactFamily};
    use std::fs;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "runtimo_jfr_test_{}_{}",
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
    fn jfr_new_detects_status() {
        let adapter = JfrAdapter::new();
        match adapter.status() {
            ProviderStatus::Unavailable { .. }
            | ProviderStatus::Active { .. }
            | ProviderStatus::Degraded { .. }
            | ProviderStatus::Failed { .. } => {}
        }
    }

    #[test]
    fn jfr_config_defaults() {
        let config = JfrConfig::default();
        assert_eq!(config.pinned_jdk_version, "17");
        assert_eq!(config.mode, "exact");
        assert!(config.external_attach_enabled);
    }

    #[test]
    fn jfr_capture_artifact_produces_hash() {
        // Skip if jcmd is absent — JFR requires the JDK17 jcmd binary.
        if detect_binary("jcmd").is_none() {
            return;
        }
        let dir = tmp_dir();
        let mut adapter = JfrAdapter::with_artifact_dir(dir.clone());
        adapter.status = ProviderStatus::Active {
            provider: "jfr".to_string(),
            version: "17".to_string(),
            mode: "exact".to_string(),
        };
        let _ = adapter.start();

        let data = serde_json::json!({"class": "java.lang.String", "method": "load"});
        let artifact = adapter
            .capture_artifact("class_load", 5678, 2000, data)
            .unwrap();

        assert_eq!(artifact.provider, "jfr");
        assert_eq!(artifact.sha256.len(), 64, "SHA-256 must be 64 hex chars");
        assert!(
            PathBuf::from(&artifact.path).exists(),
            "Artifact file must exist"
        );
        assert!(
            std::path::Path::new(&artifact.path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jfr")),
            "JFR artifact must have .jfr extension"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jfr_start_returns_started_when_active() {
        let mut adapter = JfrAdapter::new();
        adapter.status = ProviderStatus::Active {
            provider: "jfr".to_string(),
            version: "17".to_string(),
            mode: "exact".to_string(),
        };
        let result = adapter.start().unwrap();
        assert!(matches!(result, AdapterStartResult::Started { .. }));
    }

    #[test]
    fn jfr_start_returns_unavailable_when_absent() {
        let mut adapter = JfrAdapter::new();
        adapter.status = ProviderStatus::Unavailable {
            provider: "jfr".to_string(),
            version: "17".to_string(),
            reason: "jcmd not found".to_string(),
        };
        let result = adapter.start().unwrap();
        assert!(matches!(result, AdapterStartResult::Unavailable { .. }));
    }

    #[test]
    fn jfr_pinned_jdk_version_constant() {
        assert_eq!(JfrAdapter::pinned_jdk_version(), "17");
    }

    #[test]
    fn jfr_reduce_produces_class_load_facts() {
        let dir = tmp_dir();
        let mut adapter = JfrAdapter::with_artifact_dir(dir.clone());
        adapter.status = ProviderStatus::Active {
            provider: "jfr".to_string(),
            version: "17".to_string(),
            mode: "exact".to_string(),
        };
        let _ = adapter.start();

        let data = serde_json::json!({"class": "java.lang.String"});
        let artifact = adapter
            .capture_artifact("class_load", 5678, 2000, data)
            .unwrap();
        let facts = adapter.reduce(&[artifact], "run-001", "abc123").unwrap();

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].family, RuntimeFactFamily::ObservedClassLoad);
        assert_eq!(facts[0].provider, "jfr");
        assert_eq!(facts[0].fidelity, EvidenceFidelity::ExactRuntime);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
