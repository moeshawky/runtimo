//! Run manifest V1 — stable versioned run-attestation.
//!
//! # Ownership
//! - **Writer**: Executor
//! - **Reader**: WAL, CLI
//! - **Serialization**: serde JSON
//! - **Versioning**: `schema_version` field (string `"1"`)
//!
//! # Invariants
//! - `schema_version` is always `"1"` for this version.
//! - No raw environment variables or secrets are captured.
//! - Environmental identity is hashed or whitelisted.
//! - Provider list contains only providers with demonstrated consumers.

use serde::{Deserialize, Serialize};

/// Repository identity in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct RepositoryIdentity {
    /// Repository identity string.
    pub identity: String,
    /// Commit hash.
    pub commit: String,
    /// Tree hash.
    pub tree_hash: String,
    /// Whether the working tree is dirty.
    pub dirty: bool,
    /// Diff hash (hash of changes relative to commit).
    pub diff_hash: String,
}

/// Target of a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Target {
    /// Executable path.
    pub executable: String,
    /// Command-line arguments.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: String,
}

/// Process information in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct ProcessInfo {
    /// Process ID (never alone — paired with `process_start_time`).
    pub pid: u32,
    /// Process start time from `/proc/pid/stat` field 22.
    pub process_start_time: u64,
}

/// Runtime environment in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Environment {
    /// Kernel version.
    pub kernel: String,
    /// Architecture.
    pub arch: String,
    /// Runtime versions.
    pub runtimes: Vec<RuntimeVersion>,
}

/// A runtime version entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct RuntimeVersion {
    /// Runtime name (e.g., "jvm", "python", "node").
    pub name: String,
    /// Runtime version string.
    pub version: String,
}

/// Provider entry in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct ProviderEntry {
    /// Provider name.
    pub name: String,
    /// Provider version.
    pub version: String,
    /// Operating mode.
    pub mode: String,
    /// Configuration value (serialized).
    pub config: serde_json::Value,
    /// Binary hash (SHA-256 hex).
    pub binary_hash: String,
}

/// Timing information in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Timing {
    /// Start time (Unix timestamp).
    pub start_time: u64,
    /// End time (Unix timestamp).
    pub end_time: u64,
}

/// Artifact reference in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct ArtifactRef {
    /// Artifact path.
    pub path: String,
    /// Media type.
    pub media: String,
    /// SHA-256 hash.
    pub sha256: String,
    /// Provider that produced this artifact.
    pub provider: String,
}

/// Completeness information in a run manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Completeness {
    /// Provider status map.
    pub provider_status: std::collections::HashMap<String, serde_json::Value>,
    /// Watermark string.
    pub watermark: String,
}

/// Run manifest V1 — stable versioned run-attestation.
///
/// # Schema version
/// Always `schema_version: "1"` for this version.
///
/// # Invariants
/// - `schema_version` is always `"1"`.
/// - No raw env/secrets are captured indiscriminately.
/// - Every field has a demonstrated consumer.
/// - `process` always includes `process_start_time` (PID never alone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct RunManifestV1 {
    /// Schema version. Always `"1"`.
    pub schema_version: String,
    /// Unique run identifier.
    pub run_id: String,
    /// Repository identity.
    pub repository: RepositoryIdentity,
    /// Target of the run.
    pub target: Target,
    /// Process information (PID + start_time).
    pub process: ProcessInfo,
    /// Runtime environment.
    pub environment: Environment,
    /// Providers involved in this run.
    pub providers: Vec<ProviderEntry>,
    /// Timing information.
    pub timing: Timing,
    /// Artifacts produced by this run.
    pub artifacts: Vec<ArtifactRef>,
    /// Completeness information.
    pub completeness: Completeness,
}

impl RunManifestV1 {
    /// Creates a new `RunManifestV1` with `schema_version: "1"`.
    #[must_use]
    pub fn new(run_id: String, repository: RepositoryIdentity, target: Target) -> Self {
        Self {
            schema_version: "1".to_string(),
            run_id,
            repository,
            target,
            process: ProcessInfo {
                pid: 0,
                process_start_time: 0,
            },
            environment: Environment {
                kernel: String::new(),
                arch: String::new(),
                runtimes: Vec::new(),
            },
            providers: Vec::new(),
            timing: Timing {
                start_time: 0,
                end_time: 0,
            },
            artifacts: Vec::new(),
            completeness: Completeness {
                provider_status: std::collections::HashMap::new(),
                watermark: String::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest() -> RunManifestV1 {
        RunManifestV1 {
            schema_version: "1".to_string(),
            run_id: "run-001".to_string(),
            repository: RepositoryIdentity {
                identity: "runtimo".to_string(),
                commit: "abc123".to_string(),
                tree_hash: "def456".to_string(),
                dirty: false,
                diff_hash: "ghi789".to_string(),
            },
            target: Target {
                executable: "/usr/bin/runtimo".to_string(),
                argv: vec!["--mode".to_string(), "test".to_string()],
                cwd: "/tmp".to_string(),
            },
            process: ProcessInfo {
                pid: 1234,
                process_start_time: 1000,
            },
            environment: Environment {
                kernel: "5.15.0".to_string(),
                arch: "x86_64".to_string(),
                runtimes: vec![RuntimeVersion {
                    name: "jvm".to_string(),
                    version: "17.0.1".to_string(),
                }],
            },
            providers: vec![ProviderEntry {
                name: "tetragon".to_string(),
                version: "1.7.1".to_string(),
                mode: "monitor".to_string(),
                config: serde_json::json!({"level": "info"}),
                binary_hash: "a".repeat(64),
            }],
            timing: Timing {
                start_time: 1000,
                end_time: 2000,
            },
            artifacts: vec![ArtifactRef {
                path: "/tmp/bundle.jsonl".to_string(),
                media: "application/json".to_string(),
                sha256: "b".repeat(64),
                provider: "tetragon".to_string(),
            }],
            completeness: Completeness {
                provider_status: std::collections::HashMap::new(),
                watermark: "complete".to_string(),
            },
        }
    }

    #[test]
    fn run_manifest_round_trip() {
        let manifest = make_manifest();
        let json = serde_json::to_string(&manifest).unwrap();
        let deserialized: RunManifestV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, deserialized);
    }

    #[test]
    fn run_manifest_schema_version() {
        let manifest = make_manifest();
        assert_eq!(manifest.schema_version, "1");
    }

    #[test]
    fn run_manifest_unknown_field_ignored() {
        // Forward-compat: unknown fields are ignored.
        let json = r#"{"schema_version":"1","run_id":"run-001","unknown_field":"ignored","repository":{"identity":"runtimo","commit":"abc123","tree_hash":"def456","dirty":false,"diff_hash":"ghi789"},"target":{"executable":"/usr/bin/runtimo","argv":[],"cwd":"/tmp"},"process":{"pid":1234,"process_start_time":1000},"environment":{"kernel":"5.15.0","arch":"x86_64","runtimes":[]},"providers":[],"timing":{"start_time":1000,"end_time":2000},"artifacts":[],"completeness":{"provider_status":{},"watermark":""}}"#;
        let manifest: RunManifestV1 = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.run_id, "run-001");
        assert_eq!(manifest.schema_version, "1");
    }

    #[test]
    fn run_manifest_pid_never_alone() {
        // Invariant: process always has process_start_time.
        let manifest = make_manifest();
        assert!(
            manifest.process.process_start_time > 0,
            "PID must never be alone; process_start_time must be present"
        );
    }
}
